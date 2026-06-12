use chrono::{DateTime, Local};
use jwalk::{Parallelism, WalkDir};
use regex::{Regex, RegexBuilder};
use rusqlite::{params, params_from_iter, Connection};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{unbounded, Sender};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

const VERSION: &str = "0.8.6";
const NTFS_FS_TYPES: [&str; 3] = ["ntfs", "ntfs3", "fuseblk"];
const ROOT_SIZE_SKIP_TREES: [&str; 6] = ["/mnt", "/media", "/dev", "/proc", "/sys", "/run"];
const SNAPSHOT_REFRESH_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const SNAPSHOT_REFRESH_MIN_AGE: Duration = Duration::from_secs(30);
const INDEX_REFRESH_MIN_AGE: Duration = Duration::from_secs(30);
const INDEX_STREAM_FLUSH_LINES: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypeFlag {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortField {
    Date,
    Size,
    Name,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortOrder {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug)]
struct NamePattern {
    type_flag: Option<TypeFlag>,
    regex: String,
}

#[derive(Clone, Debug)]
enum SearchDirMode {
    Path(String),
    Pattern(String),
}

#[derive(Clone, Debug)]
struct MountInfo {
    device: PathBuf,
    mount_point: PathBuf,
    fs_type: String,
}

#[derive(Clone, Debug)]
struct Options {
    timeout_dur: Duration,
    timeout_explicit: bool,
    force_pattern_mode: bool,
    long_format: bool,
    long_extended: bool,
    sizes: bool,
    counts: bool,
    regex_mode: bool,
    sort_field: Option<SortField>,
    sort_order: Option<SortOrder>,
    no_recurse: bool,
    follow_links: bool,
    respect_ignore: bool,
    visible_only: bool,
    threads_override: usize,
    threads_explicit: bool,
    cache_output: bool,
    snapshot_cache: bool,
    snapshot_refresh: bool,
    index_mode: bool,
    index_binary: bool,
    index_refresh: Option<String>,
    index_purge: Option<String>,
    absolute_paths: bool,
    force_dir: bool,
    force_file: bool,
    force_full: bool,
    classify: bool,
    color_when: ColorWhen,
    hyperlinks: bool,
    highlight_match: bool,
    contains_all: bool,
    path_override: Option<String>,
    positional: Vec<String>,
}

struct SearchResult {
    path: String,
    is_dir: bool,
    is_symlink: bool,
    metadata: Option<fs::Metadata>,
}

struct SearchRun {
    lines: Vec<String>,
    timed_out: bool,
}

#[derive(Clone, Debug)]
struct ContainsAllSpec {
    terms: Vec<String>,
    root: PathBuf,
}

#[derive(Clone, Debug)]
struct HighlightSpec {
    prefix_rules: Vec<Regex>,
    leaf_rules: Vec<Regex>,
}

#[derive(Clone, Debug)]
struct DirStats {
    files: u64,
    bytes: u64,
    human: String,
}

#[derive(Default)]
struct DirStatsCache {
    map: HashMap<String, DirStats>,
    bytes_map: HashMap<String, u64>,
}

struct RawCacheState {
    dirs: BufWriter<File>,
    files: BufWriter<File>,
    seen_dirs: HashSet<String>,
    seen_files: HashSet<String>,
}

#[derive(Clone)]
struct ColorSpec {
    by_key: HashMap<String, String>,
    globs: Vec<(Regex, String)>,
    color_prefix_dir: String,
    color_dir: String,
    color_link: String,
    color_exec: String,
}

fn usage() -> String {
    let txt = r#"A parallel recursive file searcher (unearth)

Usage:
  unearth <filename/dirname> [<search_dir>]
  unearth (--full|-F) <pattern1>  [<pattern2> <pattern3>...]
                       [--dir|-d] [--file|-f] [--regex|-r] [--bypass|-b]
                       [--classify|-C]
                       [--absolute-paths|-A]
                       [--counts]
                       [--long|-l] [--long-true-dirsize|-L]
                       [--sizes]
                       [--contains-all]
                       [--path DIR]
                       [--timeout N] [--sort date|size|name asc|desc]
                       [--no-recurse|-R] [--follow-links]
                       [--ignore] [--hidden|-H] [--threads N]
                       [--cache-raw] [--snapshot-cache]
                       [--index] [--index-binary]
                       [--index-refresh DIR] [--index-purge DIR]
                       [--color=auto|always|never] [--hyperlink]
                       [--highlight-match|--match-red]
  unearth (--version|-V)

Arguments:
   <filename/dirname>:
      The file or directory name to search for. Supports exact and partial
      matching by default; use --regex/-r for regex matching.

   SEARCH MATRIX:

   Goal           | Shorthand        | Wildcard Format        | Regex Format
   ---------------|------------------|------------------------|-------------------
   Contains (All) | abc              | "*abc*"                | -r "abc"
   Contains (File)| abc -f           | "*abc*" -f             | -r "abc" -f
   Contains (Dir) | abc -d           | "*abc*" -d             | -r "abc" -d
   Exact (All)    | -                | -                      | -r "^abc$"
   Exact (File)   | -                | -                      | -r "^abc$" -f
   Exact (Dir)    | /abc/            | -                      | -r "^abc$" -d
   Starts (All)   | /abc             | "abc*"                 | -r "^abc"
   Starts (File)  | /abc -f          | "abc*" -f              | -r "^abc" -f
   Starts (Dir)   | /abc -d          | "abc*" -d              | -r "^abc" -d
   Ends (All)     | -                | "*abc"                 | -r "abc$"
   Ends (File)    | -                | "*abc" -f              | -r "abc$" -f
   Ends (Dir)     | abc/             | "*abc" -d              | -r "abc$" -d

   <search_dir>:
      Location to search. Defaults to '.' (the current directory).
      Behavior follows this priority:
      1. Local/Absolute Path: If the path exists on disk (e.g., '.', '/',
         or a specific path), the search is limited to that directory and
         will not fallback to a global search.
      2. Global Pattern Match: If the path does not exist, the script
         searches the ENTIRE disk for all directories matching the pattern
         (see matrix below) and searches inside them.

   SEARCH DIR MATRIX:

   Goal           | Shorthand | Wildcard Format | Regex Format
   ---------------|-----------|-----------------|----------------
   Contains       | abc       | "*abc*"         | -r "abc"
   Exact          | /abc/     | -               | -r "^abc$"
   Starts         | /abc      | "abc*"          | -r "^abc"
   Ends           | abc/      | "*abc"          | -r "abc$"

   Note: If the 1st check (Literal Path) fails, the script performs a global
   directory match pass before searching within matched directories.

   The --full flag matches against the full absolute path instead of just
   the basename.

   Example: unearth --full "src" "main"   # Matches BOTH
   Example: unearth --full "test"         # Matches any full path containing "test"

   Notes:
  - Use quotes around patterns containing $ or * to prevent shell expansion.
  - Regex mode is only enabled with --regex/-r.
  - --highlight-match (alias: --match-red) shows matched text in red inside
    output paths.
  - --counts prints match counts grouped by parent folder.
    This mode outputs summary rows instead of full match paths.
  - --snapshot-cache prints the last complete snapshot for this exact command
    immediately, then refreshes that snapshot in the background. Timed-out
    scans do not replace an existing snapshot. Background refreshes use a long
    timeout by default unless --timeout is passed explicitly.
  - --index queries the global pooled path database in
    ~/.cache/unearth/index/unearth.db instead of walking the filesystem. It
    returns current DB rows immediately and starts one background refresh when
    the root is missing or stale.
  - --index-binary writes --index results as repeated little-endian
    u32-length-prefixed path bytes instead of newline-delimited text.
  - --index-refresh DIR rebuilds the indexed rows for DIR in that global
    database. Directory paths and repeated entry names are stored once and
    entries link to them by integer IDs.
  - --index-purge DIR removes indexed rows for DIR and all indexed children.
    Existing roots are canonicalized; missing roots are normalized lexically.
  - --sizes prints compact sizes as SIZE<TAB>PATH (max 6 chars including
    unit, e.g., 1.111M, 111.1M),
    using recursive directory totals for directory matches.
    For top-level system trees under / (/mnt, /media, /dev, /proc, /sys, /run),
    size is shown as '-' to avoid expensive recursive traversal.
  - Name contains-all mode is implicit with 2+ plain positional terms
    (legacy name+search_dir selector forms still use search_dir mode),
    or enabled by --contains-all:
    unearth WORD1 WORD2 [WORD3 ...] [PATH]
    It finds filenames/paths containing all words in any order.
    PATH is implicit only if the last arg is absolute (/x), explicit relative
    (./x, ../x, ~/x), or contains a slash (a/b). A bare token like "folder1"
    is treated as a word unless --path folder1 is used.
  - On NTFS-like filesystems (ntfs, ntfs3, fuseblk), recursive directory size
    scans attempt an MFT fast path and fall back automatically.
    Set UNEARTH_NTFS_DEBUG=1 to print fast-path status.
  - Plain patterns are contains. For exact matches use regex anchors
    (e.g., --regex "^word$"), or /word/ for exact-directory shorthand.
"#;
    txt.to_string()
}

fn parse_duration(t: &str) -> Result<Duration, String> {
    let re = Regex::new(r"^(\d+)([sm])?$").unwrap();
    if let Some(caps) = re.captures(t) {
        let n = caps[1].parse::<u64>().map_err(|e| e.to_string())?;
        match caps.get(2).map(|m| m.as_str()) {
            Some("m") => Ok(Duration::from_secs(n * 60)),
            _ => Ok(Duration::from_secs(n)),
        }
    } else {
        Err(format!("Invalid timeout format: {}", t))
    }
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        timeout_dur: Duration::from_secs(6),
        timeout_explicit: false,
        force_pattern_mode: false,
        long_format: false,
        long_extended: false,
        sizes: false,
        counts: false,
        regex_mode: false,
        sort_field: None,
        sort_order: None,
        no_recurse: false,
        follow_links: false,
        respect_ignore: false,
        visible_only: true,
        threads_override: 8,
        threads_explicit: false,
        cache_output: false,
        snapshot_cache: false,
        snapshot_refresh: false,
        index_mode: false,
        index_binary: false,
        index_refresh: None,
        index_purge: None,
        absolute_paths: false,
        force_dir: false,
        force_file: false,
        force_full: false,
        classify: false,
        color_when: ColorWhen::Auto,
        hyperlinks: false,
        highlight_match: false,
        contains_all: false,
        path_override: None,
        positional: Vec::new(),
    };

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];

        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            let mut all_known = true;
            for ch in arg[1..].chars() {
                let handled = match ch {
                    'd' => {
                        opts.force_dir = true;
                        true
                    }
                    'f' => {
                        opts.force_file = true;
                        true
                    }
                    'F' => {
                        opts.force_full = true;
                        true
                    }
                    'C' => {
                        opts.classify = true;
                        true
                    }
                    'A' => {
                        opts.absolute_paths = true;
                        true
                    }
                    'r' => {
                        opts.regex_mode = true;
                        true
                    }
                    'R' => {
                        opts.no_recurse = true;
                        true
                    }
                    'H' => {
                        opts.visible_only = false;
                        true
                    }
                    'b' => {
                        opts.force_pattern_mode = true;
                        true
                    }
                    'l' => {
                        opts.long_format = true;
                        true
                    }
                    'L' => {
                        opts.long_format = true;
                        opts.long_extended = true;
                        true
                    }
                    'h' => {
                        print!("{}", usage());
                        std::process::exit(0);
                    }
                    'V' => {
                        println!("unearth {}", VERSION);
                        std::process::exit(0);
                    }
                    _ => false,
                };
                if !handled {
                    all_known = false;
                    break;
                }
            }

            if all_known {
                i += 1;
                continue;
            }
        }

        match arg.as_str() {
            "--timeout" => {
                i += 1;
                if i < args.len() {
                    opts.timeout_dur = parse_duration(&args[i])?;
                    opts.timeout_explicit = true;
                }
            }
            _ if arg.starts_with("--timeout=") => {
                opts.timeout_dur = parse_duration(arg.trim_start_matches("--timeout="))?;
                opts.timeout_explicit = true;
            }
            "--threads" => {
                i += 1;
                if i < args.len() {
                    opts.threads_override = args[i]
                        .parse::<usize>()
                        .map_err(|_| "Invalid threads count")?;
                    if opts.threads_override == 0 {
                        return Err("--threads requires a positive integer".to_string());
                    }
                    opts.threads_explicit = true;
                }
            }
            _ if arg.starts_with("--threads=") => {
                opts.threads_override = arg
                    .trim_start_matches("--threads=")
                    .parse::<usize>()
                    .map_err(|_| "Invalid threads count")?;
                if opts.threads_override == 0 {
                    return Err("--threads requires a positive integer".to_string());
                }
                opts.threads_explicit = true;
            }
            "--color" => {
                i += 1;
                if i < args.len() {
                    opts.color_when = parse_color_when(&args[i])?;
                }
            }
            _ if arg.starts_with("--color=") => {
                opts.color_when = parse_color_when(arg.trim_start_matches("--color="))?;
            }
            "--hyperlink" => opts.hyperlinks = true,
            "--highlight-match" | "--match-red" => opts.highlight_match = true,
            "--contains-all" => opts.contains_all = true,
            "--path" => {
                i += 1;
                if i < args.len() {
                    opts.path_override = Some(args[i].clone());
                } else {
                    return Err("--path requires a directory argument".to_string());
                }
            }
            _ if arg.starts_with("--path=") => {
                let v = arg.trim_start_matches("--path=").to_string();
                if v.is_empty() {
                    return Err("--path requires a non-empty directory argument".to_string());
                }
                opts.path_override = Some(v);
            }
            "--dir" | "-d" => opts.force_dir = true,
            "--file" | "-f" => opts.force_file = true,
            "--full" | "-F" => opts.force_full = true,
            "--counts" => opts.counts = true,
            "--classify" | "-C" => opts.classify = true,
            "--absolute-paths" | "-A" => opts.absolute_paths = true,
            "--regex" | "-r" => opts.regex_mode = true,
            "--sort" => {
                if i + 2 < args.len() {
                    let field = args[i + 1].as_str();
                    let order = args[i + 2].as_str();
                    opts.sort_field = match field {
                        "date" => Some(SortField::Date),
                        "size" => Some(SortField::Size),
                        "name" => Some(SortField::Name),
                        _ => return Err(format!("Unsupported sort field '{}'", field)),
                    };
                    opts.sort_order = match order {
                        "asc" => Some(SortOrder::Asc),
                        "desc" => Some(SortOrder::Desc),
                        _ => return Err(format!("Unsupported sort order '{}'", order)),
                    };
                    i += 2;
                }
            }
            "--no-recurse" | "-R" => opts.no_recurse = true,
            "--follow-links" => opts.follow_links = true,
            "--ignore" => opts.respect_ignore = true,
            "--hidden" | "-H" => opts.visible_only = false,
            "--cache-raw" => opts.cache_output = true,
            "--snapshot-cache" => opts.snapshot_cache = true,
            "--snapshot-refresh" => opts.snapshot_refresh = true,
            "--index" => opts.index_mode = true,
            "--index-binary" => {
                opts.index_mode = true;
                opts.index_binary = true;
            }
            "--index-refresh" => {
                i += 1;
                if i < args.len() {
                    opts.index_refresh = Some(args[i].clone());
                } else {
                    return Err("--index-refresh requires a root path".to_string());
                }
            }
            _ if arg.starts_with("--index-refresh=") => {
                let v = arg.trim_start_matches("--index-refresh=").to_string();
                if v.is_empty() {
                    return Err("--index-refresh requires a non-empty root path".to_string());
                }
                opts.index_refresh = Some(v);
            }
            "--index-purge" => {
                i += 1;
                if i < args.len() {
                    opts.index_purge = Some(args[i].clone());
                } else {
                    return Err("--index-purge requires a root path".to_string());
                }
            }
            _ if arg.starts_with("--index-purge=") => {
                let v = arg.trim_start_matches("--index-purge=").to_string();
                if v.is_empty() {
                    return Err("--index-purge requires a non-empty root path".to_string());
                }
                opts.index_purge = Some(v);
            }
            "--cache" => return Err("--cache was renamed to --cache-raw".to_string()),
            "--bypass" | "-b" => opts.force_pattern_mode = true,
            "--long" | "-l" => opts.long_format = true,
            "--sizes" => opts.sizes = true,
            "-L" | "--long-true-dirsize" => {
                opts.long_format = true;
                opts.long_extended = true;
            }
            "--info" | "-i" => return Err("--info/-i was renamed to --long/-l".to_string()),
            "--version" | "-V" => {
                println!("unearth {}", VERSION);
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            "--" => {
                for x in args.iter().skip(i + 1) {
                    opts.positional.push(x.clone());
                }
                break;
            }
            _ => opts.positional.push(arg.clone()),
        }
        i += 1;
    }

    if opts.positional.is_empty() && opts.index_refresh.is_none() && opts.index_purge.is_none() {
        return Err(usage());
    }
    Ok(opts)
}

fn parse_color_when(v: &str) -> Result<ColorWhen, String> {
    match v {
        "auto" => Ok(ColorWhen::Auto),
        "always" => Ok(ColorWhen::Always),
        "never" => Ok(ColorWhen::Never),
        _ => Err(format!("Unsupported --color value '{}'", v)),
    }
}

fn style_enabled(opts: &Options, stdout_is_tty: bool) -> bool {
    match opts.color_when {
        ColorWhen::Auto => stdout_is_tty,
        ColorWhen::Always => true,
        ColorWhen::Never => false,
    }
}

fn can_stream_direct(opts: &Options, use_style: bool) -> bool {
    !use_style
        && !opts.classify
        && !opts.force_full
        && !opts.highlight_match
        && !opts.counts
        && opts.sort_field.is_none()
        && !opts.long_format
        && !opts.sizes
        && !opts.snapshot_cache
        && !opts.snapshot_refresh
        && !opts.absolute_paths
}

fn write_binary_path_record<W: Write>(writer: &mut W, path: &str) -> io::Result<()> {
    let len = u32::try_from(path.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "path too long"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(path.as_bytes())
}

fn escape_regex_keep_star(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if "[](){}.^$|+?".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn to_regex_fragment(s: &str) -> String {
    let mut x = escape_regex_keep_star(s);
    x = x.replace(r"\*", "__LITERAL_STAR__");
    x = x.replace('*', ".*");
    x.replace("__LITERAL_STAR__", r"\*")
}

fn wildcard_to_regex(pat: &str) -> String {
    let lead_star = pat.starts_with('*');
    let trail_star =
        pat.ends_with('*') && (pat.len() < 2 || pat.as_bytes()[pat.len() - 2] != b'\\');
    let mut rx = to_regex_fragment(pat);
    if !lead_star {
        rx = format!("^{}", rx);
    }
    if !trail_star {
        rx.push('$');
    }
    rx
}

fn is_wrapped_quote(raw: &str) -> bool {
    (raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2)
        || (raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2)
}

fn parse_name_pattern(raw: &str, regex_mode: bool) -> NamePattern {
    let mut out = NamePattern {
        type_flag: None,
        regex: String::new(),
    };
    if is_wrapped_quote(raw) {
        let mut inner = raw[1..raw.len() - 1].to_string();
        inner = inner.trim_start_matches('/').to_string();
        if inner != "/" {
            inner = inner.trim_end_matches('/').to_string();
        }
        out.regex = if regex_mode {
            inner
        } else {
            wildcard_to_regex(&inner)
        };
        return out;
    }
    if regex_mode {
        out.regex = raw.to_string();
        return out;
    }
    if raw.starts_with('/') && raw.ends_with('/') {
        let frag = raw[1..raw.len() - 1].to_string();
        out.type_flag = Some(TypeFlag::Dir);
        out.regex = format!("^{}$", to_regex_fragment(&frag));
        return out;
    }
    if raw.starts_with('/') {
        let frag = raw.trim_start_matches('/');
        out.regex = format!("^{}", to_regex_fragment(frag));
        return out;
    }
    if raw != "/" && raw.ends_with('/') {
        out.type_flag = Some(TypeFlag::Dir);
        let no_slash = raw.trim_end_matches('/');
        out.regex = format!("{}$", to_regex_fragment(no_slash));
        return out;
    }
    if raw.contains('*') {
        out.regex = wildcard_to_regex(raw);
        return out;
    }
    out.regex = to_regex_fragment(raw);
    out
}

fn pattern_prefers_full_path(raw: &str, regex_mode: bool) -> bool {
    if regex_mode {
        return true;
    }
    let token = if is_wrapped_quote(raw) && raw.len() >= 2 {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    token.contains('/')
}

fn term_selectivity_score(raw: &str, regex_mode: bool) -> i64 {
    let mut score: i64 = 0;
    let core = if is_wrapped_quote(raw) && raw.len() >= 2 {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    let meaningful_len = core
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .count() as i64;
    score += meaningful_len * 24;

    if regex_mode {
        if raw.starts_with('^') {
            score += 1200;
        }
        if raw.ends_with('$') {
            score += 1200;
        }
        if raw.contains(".*") || raw.contains(".+") {
            score -= 900;
        }
        if raw.contains('|') {
            score -= 600;
        }
        let heavy_meta = raw
            .chars()
            .filter(|c| matches!(c, '[' | ']' | '(' | ')' | '{' | '}' | '?' | '+'))
            .count() as i64;
        score -= heavy_meta * 60;
        return score;
    }

    if is_wrapped_quote(raw) {
        score += 2200;
    }
    if raw.starts_with('/') && raw.ends_with('/') {
        score += 1700;
    } else if raw.starts_with('/') || (raw != "/" && raw.ends_with('/')) {
        score += 900;
    }

    let stars = raw.matches('*').count() as i64;
    if stars > 0 {
        score -= stars * 500;
        if raw == "*" {
            score -= 5000;
        }
    } else {
        score += 300;
    }

    score
}

fn canonical_path(raw: &str) -> Option<String> {
    let p = Path::new(raw);
    if p.is_dir() {
        fs::canonicalize(p)
            .ok()
            .map(|x| x.to_string_lossy().to_string())
    } else {
        None
    }
}

fn parse_search_dir(raw: &str, regex_mode: bool, force_pattern_mode: bool) -> SearchDirMode {
    if !force_pattern_mode {
        if let Some(p) = canonical_path(raw) {
            return SearchDirMode::Path(p);
        }
    }
    let mut normalized = raw.to_string();
    if normalized != "/" {
        normalized = normalized.trim_end_matches('/').to_string();
    }
    if is_wrapped_quote(raw) {
        let inner = raw[1..raw.len() - 1].to_string();
        if !force_pattern_mode {
            if let Some(p) = canonical_path(&inner) {
                return SearchDirMode::Path(p);
            }
        }
        let mut pattern_inner = inner.trim_start_matches('/').to_string();
        if pattern_inner != "/" {
            pattern_inner = pattern_inner.trim_end_matches('/').to_string();
        }
        let rx = if regex_mode {
            pattern_inner
        } else {
            wildcard_to_regex(&pattern_inner)
        };
        return SearchDirMode::Pattern(rx);
    }
    if regex_mode {
        return SearchDirMode::Pattern(normalized);
    }
    if raw.starts_with('/') && raw.ends_with('/') {
        return SearchDirMode::Pattern(format!("^{}$", to_regex_fragment(&raw[1..raw.len() - 1])));
    }
    if raw.starts_with("./") && raw.ends_with('/') {
        return SearchDirMode::Pattern(format!("^{}$", to_regex_fragment(&raw[2..raw.len() - 1])));
    }
    if raw.starts_with('/') {
        return SearchDirMode::Pattern(format!(
            "^{}",
            to_regex_fragment(raw.trim_start_matches('/'))
        ));
    }
    if raw.starts_with("./") {
        return SearchDirMode::Pattern(format!(
            "^{}",
            to_regex_fragment(raw.trim_start_matches("./"))
        ));
    }
    if raw != "/" && raw.ends_with('/') {
        return SearchDirMode::Pattern(format!(
            "{}$",
            to_regex_fragment(raw.trim_end_matches('/'))
        ));
    }
    if normalized.contains('*') {
        return SearchDirMode::Pattern(wildcard_to_regex(&normalized));
    }
    SearchDirMode::Pattern(to_regex_fragment(&normalized))
}

fn expand_home_path(raw: &str) -> String {
    if raw == "~" {
        return env::var("HOME").unwrap_or_else(|_| raw.to_string());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{}/{}", home, rest);
        }
    }
    raw.to_string()
}

fn is_implicit_content_path_token(raw: &str) -> bool {
    raw == "."
        || raw == ".."
        || raw.starts_with('/')
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.starts_with("~/")
        || raw.contains('/')
}

fn is_explicit_search_dir_selector(raw: &str) -> bool {
    if raw == "." || raw == ".." {
        return true;
    }
    if is_wrapped_quote(raw) {
        return true;
    }
    if raw.contains('*') {
        return true;
    }
    if (raw.starts_with('/') || raw.starts_with("./")) && raw.ends_with('/') {
        return true;
    }
    if raw.starts_with('/') || raw.starts_with("./") {
        return true;
    }
    raw != "/" && raw.ends_with('/')
}

fn resolve_literal_search_root(raw: &str) -> Result<PathBuf, String> {
    let expanded = expand_home_path(raw);
    let path = PathBuf::from(expanded);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "--path target '{}' is not an existing directory",
            raw
        ))
    }
}

fn contains_all_spec_from_opts(opts: &Options) -> Result<Option<ContainsAllSpec>, String> {
    let implicit_by_terms = if opts.positional.len() >= 3 {
        true
    } else if opts.positional.len() == 2 {
        let first = &opts.positional[0];
        let second = &opts.positional[1];
        !opts.regex_mode
            && !opts.force_pattern_mode
            && !is_wrapped_quote(first)
            && !is_explicit_search_dir_selector(second)
    } else {
        false
    };
    let forced_by_flags = opts.contains_all || opts.path_override.is_some();
    if opts.force_full && !forced_by_flags && !implicit_by_terms {
        return Ok(None);
    }
    if !(forced_by_flags || implicit_by_terms) {
        return Ok(None);
    }
    let mut terms = opts.positional.clone();
    let mut root_raw = opts.path_override.clone();
    if root_raw.is_none() && !terms.is_empty() {
        if let Some(last) = terms.last() {
            if is_implicit_content_path_token(last) {
                root_raw = Some(last.clone());
                terms.pop();
            }
        }
    }
    if terms.is_empty() {
        return Err("contains-all mode requires at least one search term".to_string());
    }
    let root = if let Some(raw) = root_raw {
        resolve_literal_search_root(&raw)?
    } else {
        PathBuf::from(".")
    };
    Ok(Some(ContainsAllSpec { terms, root }))
}

struct PathInfo {
    path: PathBuf,
    is_dir: bool,
}

#[derive(Default)]
struct SimpleIgnoreRules {
    names: HashSet<String>,
    dir_names: HashSet<String>,
}

fn load_simple_ignore_rules(dir: &Path) -> SimpleIgnoreRules {
    let mut rules = SimpleIgnoreRules::default();
    for ignore_name in [".gitignore", ".ignore", ".fdignore"] {
        let path = dir.join(ignore_name);
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let mut token = line;
            let is_dir_only = token.ends_with('/');
            if is_dir_only {
                token = token.trim_end_matches('/');
            }
            if token.is_empty() {
                continue;
            }
            if token.contains('/')
                || token.contains('*')
                || token.contains('?')
                || token.contains('[')
            {
                continue;
            }
            if is_dir_only {
                rules.dir_names.insert(token.to_string());
            } else {
                rules.names.insert(token.to_string());
            }
        }
    }
    rules
}

fn is_simple_ignored_name(name: &str, is_dir: bool, rules: &SimpleIgnoreRules) -> bool {
    rules.names.contains(name) || (is_dir && rules.dir_names.contains(name))
}

fn walk_fast(
    dir: PathBuf,
    re: &Regex,
    is_catch_all: bool,
    tx: &Sender<Vec<PathInfo>>,
    visible_only: bool,
    respect_ignore: bool,
    no_recurse: bool,
    follow_links: bool,
    type_flag: Option<TypeFlag>,
    full_path_match: bool,
    serial_subtree: bool,
    timeout_flag: &Arc<AtomicBool>,
) {
    if timeout_flag.load(Ordering::Relaxed) {
        return;
    }
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return;
    };
    let ignore_rules = if respect_ignore {
        Some(load_simple_ignore_rules(&dir))
    } else {
        None
    };
    let mut subdirs = Vec::new();
    let mut local_buf = Vec::with_capacity(512);
    for entry_res in read_dir {
        let Ok(entry) = entry_res else { continue };
        let name = entry.file_name();
        let name_bytes = name.as_bytes();
        if visible_only && name_bytes.starts_with(b".") {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let is_symlink = file_type.is_symlink();
        let mut is_dir = file_type.is_dir();
        if is_symlink && follow_links {
            if let Ok(meta) = fs::metadata(&path) {
                if meta.is_dir() {
                    is_dir = true;
                }
            }
        }
        let name_lossy = name.to_string_lossy();
        if let Some(rules) = &ignore_rules {
            if is_simple_ignored_name(&name_lossy, is_dir, rules) {
                continue;
            }
        }
        let skip_type = match type_flag {
            Some(TypeFlag::File) => is_dir,
            Some(TypeFlag::Dir) => !is_dir,
            None => false,
        };
        if !skip_type {
            let is_match = if is_catch_all {
                true
            } else {
                let match_target = if full_path_match {
                    path.to_string_lossy()
                } else {
                    name.to_string_lossy()
                };
                re.is_match(&match_target)
            };
            if is_match {
                local_buf.push(PathInfo {
                    path: path.clone(),
                    is_dir,
                });
                if local_buf.len() >= 512 {
                    if tx.send(std::mem::take(&mut local_buf)).is_err() {
                        return;
                    }
                    local_buf.reserve(512);
                }
            }
        }
        if is_dir && !no_recurse {
            if dir.as_os_str().as_bytes() == b"/" {
                if name_bytes == b"proc"
                    || name_bytes == b"sys"
                    || name_bytes == b"dev"
                    || name_bytes == b"run"
                {
                    continue;
                }
            }
            if is_symlink && !follow_links {
                continue;
            }
            subdirs.push(path);
        }
    }
    if !local_buf.is_empty() {
        if tx.send(local_buf).is_err() {
            return;
        }
    }
    if serial_subtree {
        for subdir in subdirs {
            walk_fast(
                subdir,
                re,
                is_catch_all,
                tx,
                visible_only,
                respect_ignore,
                no_recurse,
                follow_links,
                type_flag,
                full_path_match,
                true,
                timeout_flag,
            );
        }
    } else {
        subdirs
            .into_par_iter()
            .for_each_with(tx.clone(), |tx_clone, subdir| {
                let next_serial = root_prefers_single_thread(&subdir);
                walk_fast(
                    subdir,
                    re,
                    is_catch_all,
                    tx_clone,
                    visible_only,
                    respect_ignore,
                    no_recurse,
                    follow_links,
                    type_flag,
                    full_path_match,
                    next_serial,
                    timeout_flag,
                );
            });
    }
}

fn walk_rayon_worker(
    dir: PathBuf,
    re: &Regex,
    is_catch_all: bool,
    tx: &Sender<Vec<SearchResult>>,
    opts: &Options,
    type_flag: Option<TypeFlag>,
    full_path_match: bool,
    prune_matched_dir_subtrees: bool,
    needs_metadata: bool,
    serial_subtree: bool,
    timeout_flag: &Arc<AtomicBool>,
) {
    if timeout_flag.load(Ordering::Relaxed) {
        return;
    }
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return;
    };
    let ignore_rules = if opts.respect_ignore {
        Some(load_simple_ignore_rules(&dir))
    } else {
        None
    };
    let mut subdirs = Vec::new();
    let mut local_buf = Vec::with_capacity(256);
    for entry_res in read_dir {
        let Ok(entry) = entry_res else { continue };
        let name = entry.file_name();
        let name_lossy = name.to_string_lossy();
        if opts.visible_only && name_lossy.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let is_symlink = file_type.is_symlink();
        let mut is_dir = file_type.is_dir();
        if is_symlink && opts.follow_links {
            if let Ok(meta) = fs::metadata(&path) {
                if meta.is_dir() {
                    is_dir = true;
                }
            }
        }
        if let Some(rules) = &ignore_rules {
            if is_simple_ignored_name(&name_lossy, is_dir, rules) {
                continue;
            }
        }
        let skip_type = match type_flag {
            Some(TypeFlag::File) => is_dir,
            Some(TypeFlag::Dir) => !is_dir,
            None => false,
        };
        let mut matched_dir_pruned = false;
        if !skip_type {
            let is_match = if is_catch_all {
                true
            } else {
                if full_path_match {
                    if re.is_match(name_lossy.as_ref()) {
                        true
                    } else {
                        let match_target = path.to_string_lossy();
                        re.is_match(&match_target)
                    }
                } else {
                    re.is_match(name_lossy.as_ref())
                }
            };
            if is_match {
                let mut p_str = path
                    .into_os_string()
                    .into_string()
                    .unwrap_or_else(|os| os.to_string_lossy().into_owned());
                if is_dir && !p_str.ends_with('/') {
                    p_str.push('/');
                }
                local_buf.push(SearchResult {
                    path: p_str,
                    is_dir,
                    is_symlink,
                    metadata: if needs_metadata {
                        entry.metadata().ok()
                    } else {
                        None
                    },
                });

                if local_buf.len() >= 256 {
                    if tx.send(std::mem::take(&mut local_buf)).is_err() {
                        return;
                    }
                    local_buf.reserve(256);
                }
                if is_dir && prune_matched_dir_subtrees {
                    matched_dir_pruned = true;
                }
            }
        }
        if is_dir && !opts.no_recurse {
            if matched_dir_pruned {
                continue;
            }
            if dir.to_str() == Some("/")
                && (name_lossy == "proc"
                    || name_lossy == "sys"
                    || name_lossy == "dev"
                    || name_lossy == "run")
            {
                continue;
            }
            if is_symlink && !opts.follow_links {
                continue;
            }
            subdirs.push(entry.path());
        }
    }
    if !local_buf.is_empty() {
        if tx.send(local_buf).is_err() {
            return;
        }
    }
    if serial_subtree {
        for subdir in subdirs {
            walk_rayon_worker(
                subdir,
                re,
                is_catch_all,
                tx,
                opts,
                type_flag,
                full_path_match,
                prune_matched_dir_subtrees,
                needs_metadata,
                true,
                timeout_flag,
            );
        }
    } else {
        subdirs
            .into_par_iter()
            .for_each_with(tx.clone(), |tx_clone, subdir| {
                let next_serial = root_prefers_single_thread(&subdir);
                walk_rayon_worker(
                    subdir,
                    re,
                    is_catch_all,
                    tx_clone,
                    opts,
                    type_flag,
                    full_path_match,
                    prune_matched_dir_subtrees,
                    needs_metadata,
                    next_serial,
                    timeout_flag,
                );
            });
    }
}

fn unescape_proc_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let a = bytes[i + 1];
            let b = bytes[i + 2];
            let c = bytes[i + 3];
            let octal = (b'0'..=b'7').contains(&a)
                && (b'0'..=b'7').contains(&b)
                && (b'0'..=b'7').contains(&c);
            if octal {
                let value = ((a - b'0') << 6) | ((b - b'0') << 3) | (c - b'0');
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn detect_mount_info(path: &Path) -> Option<MountInfo> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let mut best: Option<(usize, MountInfo)> = None;
    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let (device_raw, mount_point_raw, fs_type) =
            match (parts.next(), parts.next(), parts.next()) {
                (Some(device), Some(mount), Some(fs_type)) => (device, mount, fs_type),
                _ => continue,
            };
        let device = PathBuf::from(unescape_proc_mount_field(device_raw));
        let mount_point = PathBuf::from(unescape_proc_mount_field(mount_point_raw));
        if !canonical.starts_with(&mount_point) {
            continue;
        }
        let mount_len = mount_point.as_os_str().as_bytes().len();
        if best
            .as_ref()
            .map(|(best_len, _)| mount_len > *best_len)
            .unwrap_or(true)
        {
            best = Some((
                mount_len,
                MountInfo {
                    device,
                    mount_point,
                    fs_type: fs_type.to_string(),
                },
            ));
        }
    }
    best.map(|(_, info)| info)
}

fn ntfs_best_filename(
    entry: &ntfs::NtfsIndexEntry<'_, ntfs::indexes::NtfsFileNameIndex>,
) -> Option<String> {
    if let Some(Ok(file_name)) = entry.key() {
        let name = file_name.name().to_string_lossy().to_string();
        if !name.contains('~') || name.len() > 12 {
            return Some(name);
        }
    }
    entry
        .key()
        .and_then(|result| result.ok())
        .map(|file_name| file_name.name().to_string_lossy().to_string())
}

fn ntfs_is_reparse_point(file: &ntfs::NtfsFile, device: &mut fs::File) -> bool {
    let mut attrs = file.attributes();
    while let Some(attr_result) = attrs.next(device) {
        if let Ok(attr_item) = attr_result {
            if let Ok(attr) = attr_item.to_attribute() {
                if let Ok(attr_ty) = attr.ty() {
                    if attr_ty == ntfs::NtfsAttributeType::ReparsePoint {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn ntfs_file_logical_size(file: &ntfs::NtfsFile, device: &mut fs::File) -> u64 {
    if let Some(data_attr) = file.data(device, "") {
        if let Ok(data_item) = data_attr {
            if let Ok(data_attr_obj) = data_item.to_attribute() {
                if let Ok(value) = data_attr_obj.value(device) {
                    return value.len();
                }
            }
        }
    }
    0
}

fn ntfs_find_subdir_record(
    ntfs: &ntfs::Ntfs,
    device: &mut fs::File,
    start_record: u64,
    rel_path: &Path,
) -> Option<u64> {
    let mut current_record = start_record;
    if rel_path.as_os_str().is_empty() {
        return Some(current_record);
    }
    for component in rel_path.components() {
        let name = match component {
            Component::Normal(name) => name.to_string_lossy().to_string(),
            _ => continue,
        };
        let dir_file = ntfs.file(device, current_record).ok()?;
        let index = dir_file.directory_index(device).ok()?;
        let mut entries = index.entries();
        let mut seen_records = HashSet::<u64>::new();
        let mut next_record: Option<u64> = None;
        while let Some(entry_result) = entries.next(device) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let entry_name = match ntfs_best_filename(&entry) {
                Some(n) => n,
                None => continue,
            };
            if entry_name == "." || entry_name == ".." {
                continue;
            }
            let child_record = entry.file_reference().file_record_number();
            if !seen_records.insert(child_record) {
                continue;
            }
            if entry_name == name {
                next_record = Some(child_record);
                break;
            }
        }
        current_record = next_record?;
    }
    Some(current_record)
}

fn ntfs_scan_subtree_record(
    ntfs: &ntfs::Ntfs,
    device: &mut fs::File,
    top_record: u64,
    count_files: bool,
) -> (u64, u64) {
    let mut total_size = 0u64;
    let mut total_files = 0u64;
    let mut stack = vec![top_record];
    let mut seen_dirs = HashSet::<u64>::new();
    while let Some(current_record) = stack.pop() {
        if !seen_dirs.insert(current_record) {
            continue;
        }
        let dir_file = match ntfs.file(device, current_record) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let index = match dir_file.directory_index(device) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let mut entries = index.entries();
        let mut seen_records = HashSet::<u64>::new();
        while let Some(entry_result) = entries.next(device) {
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = match ntfs_best_filename(&entry) {
                Some(n) => n,
                None => continue,
            };
            if name == "." || name == ".." {
                continue;
            }
            let child_record = entry.file_reference().file_record_number();
            if !seen_records.insert(child_record) {
                continue;
            }
            let child_file = match ntfs.file(device, child_record) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let child_is_dir = child_file.is_directory();
            let child_is_reparse = ntfs_is_reparse_point(&child_file, device);
            if child_is_dir && !child_is_reparse {
                stack.push(child_record);
            } else if !child_is_reparse {
                total_size = total_size.saturating_add(ntfs_file_logical_size(&child_file, device));
                if count_files {
                    total_files = total_files.saturating_add(1);
                }
            }
        }
    }
    (total_size, total_files)
}

fn get_dir_stats_ntfs_mft(path: &Path, count_files: bool) -> io::Result<(u64, u64)> {
    let canonical_base = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mount = detect_mount_info(&canonical_base)
        .ok_or_else(|| io::Error::other("mount detection failed"))?;
    if !NTFS_FS_TYPES.iter().any(|t| mount.fs_type == *t) {
        return Err(io::Error::other("not ntfs"));
    }
    let mut device = fs::File::open(&mount.device)?;
    let ntfs = ntfs::Ntfs::new(&mut device).map_err(|err| io::Error::other(err.to_string()))?;
    let root_dir = ntfs
        .root_directory(&mut device)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let root_record = root_dir.file_record_number();
    let rel_path = canonical_base
        .strip_prefix(&mount.mount_point)
        .unwrap_or(Path::new(""));
    let base_record = ntfs_find_subdir_record(&ntfs, &mut device, root_record, rel_path)
        .ok_or_else(|| io::Error::other("base directory not found in mft"))?;
    Ok(ntfs_scan_subtree_record(
        &ntfs,
        &mut device,
        base_record,
        count_files,
    ))
}

fn get_dir_stats_walk(path: &str, count_files: bool) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    for entry_res in WalkDir::new(path).follow_links(false) {
        let Ok(entry) = entry_res else { continue };
        let file_type = entry.file_type();
        if !file_type.is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            bytes = bytes.saturating_add(meta.len());
        }
        if count_files {
            files = files.saturating_add(1);
        }
    }
    (bytes, files)
}

fn get_dir_stats_native(path: &str, count_files: bool) -> (u64, u64) {
    let path_buf = PathBuf::from(path);
    let ntfs_debug = env::var_os("UNEARTH_NTFS_DEBUG").is_some();
    match get_dir_stats_ntfs_mft(&path_buf, count_files) {
        Ok(stats) => {
            if ntfs_debug {
                eprintln!("unearth: NTFS MFT fast path enabled for {}", path);
            }
            stats
        }
        Err(err) => {
            if ntfs_debug
                && detect_mount_info(&path_buf)
                    .as_ref()
                    .map(|m| NTFS_FS_TYPES.iter().any(|t| m.fs_type == *t))
                    .unwrap_or(false)
            {
                eprintln!(
                    "unearth: NTFS MFT fast path unavailable for {}: {}",
                    path, err
                );
            }
            get_dir_stats_walk(path, count_files)
        }
    }
}

fn get_dir_bytes_native_serial(path: &str) -> u64 {
    if let Ok((bytes, _)) = get_dir_stats_ntfs_mft(Path::new(path), false) {
        return bytes;
    }
    let mut bytes = 0u64;
    for entry_res in WalkDir::new(path)
        .follow_links(false)
        .parallelism(Parallelism::Serial)
    {
        let Ok(entry) = entry_res else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            bytes = bytes.saturating_add(meta.len());
        }
    }
    bytes
}

fn normalize_dir_key(path: &str) -> String {
    if path == "/" {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    }
}

fn should_skip_root_size_tree(path: &str) -> bool {
    let normalized = normalize_dir_key(path);
    ROOT_SIZE_SKIP_TREES
        .iter()
        .any(|prefix| normalized == *prefix || normalized.starts_with(&format!("{}/", prefix)))
}

fn root_prefers_single_thread(path: &Path) -> bool {
    path == Path::new("/media") || path.starts_with("/media/")
}

fn effective_search_root(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
) -> Option<PathBuf> {
    if let Some(spec) = content_spec {
        return fs::canonicalize(&spec.root)
            .ok()
            .or_else(|| Some(spec.root.clone()));
    }
    if opts.force_full {
        if opts.positional.len() > 1 {
            if let Some(last) = opts.positional.last() {
                if Path::new(last).is_dir() {
                    return fs::canonicalize(last)
                        .ok()
                        .or_else(|| Some(PathBuf::from(last)));
                }
            }
        }
        return env::current_dir()
            .ok()
            .and_then(|p| fs::canonicalize(p).ok().or(Some(PathBuf::from("."))));
    }
    if opts.positional.len() > 1 {
        match parse_search_dir(
            &opts.positional[1],
            opts.regex_mode,
            opts.force_pattern_mode,
        ) {
            SearchDirMode::Path(p) => {
                return fs::canonicalize(&p).ok().or_else(|| Some(PathBuf::from(p)));
            }
            SearchDirMode::Pattern(_) => return None,
        }
    }
    env::current_dir()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok().or(Some(PathBuf::from("."))))
}

fn effective_threads_override(opts: &Options, content_spec: Option<&ContainsAllSpec>) -> usize {
    if opts.threads_explicit {
        return opts.threads_override;
    }
    let Some(root) = effective_search_root(opts, content_spec) else {
        return opts.threads_override;
    };
    if root_prefers_single_thread(&root) {
        1
    } else {
        opts.threads_override
    }
}

fn format_size_iec(bytes: u64) -> String {
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

fn format_size_compact_3(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut unit = 0usize;
    let mut size = bytes as f64;
    while size >= 1024.0 && unit < units.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}{}", bytes, units[unit])
    } else {
        let decimals = if size >= 1000.0 {
            0
        } else if size >= 100.0 {
            1
        } else if size >= 10.0 {
            2
        } else {
            3
        };
        let factor = 10f64.powi(decimals as i32);
        let truncated = (size * factor).floor() / factor;
        format!("{:.*}{}", decimals, truncated, units[unit])
    }
}

fn should_use_recursive_dirsize(item: &SearchResult, opts: &Options) -> bool {
    if !item.is_dir || item.is_symlink {
        return false;
    }
    opts.sizes || opts.long_extended || !opts.no_recurse
}

fn size_bytes_for_result(item: &SearchResult, opts: &Options, cache: &mut DirStatsCache) -> u64 {
    if should_use_recursive_dirsize(item, opts) {
        if should_skip_root_size_tree(&item.path) {
            return 0;
        }
        return get_dirsize_bytes(&item.path, cache).unwrap_or(0);
    }
    item.metadata.as_ref().map(|m| m.len()).unwrap_or(0)
}

fn precompute_dirsize_cache(items: &[SearchResult], opts: &Options, cache: &mut DirStatsCache) {
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

fn sort_results(
    mut items: Vec<SearchResult>,
    opts: &Options,
    cache: &mut DirStatsCache,
) -> Vec<SearchResult> {
    let Some(field) = opts.sort_field else {
        return items;
    };
    let order = opts.sort_order.unwrap_or(SortOrder::Asc);
    items.sort_by(|a, b| {
        let ord = match field {
            SortField::Date => {
                let da = a
                    .metadata
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let db = b
                    .metadata
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                da.cmp(&db)
            }
            SortField::Size => {
                let sa = size_bytes_for_result(a, opts, cache);
                let sb = size_bytes_for_result(b, opts, cache);
                sa.cmp(&sb)
            }
            SortField::Name => a.path.to_lowercase().cmp(&b.path.to_lowercase()),
        };
        match order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        }
    });
    items
}

fn absolute_paths_transform(mut items: Vec<SearchResult>, opts: &Options) -> Vec<SearchResult> {
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

fn parent_pid() -> Option<u32> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let (_, tail) = stat.rsplit_once(") ")?;
    let mut fields = tail.split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    Some(ppid)
}

fn fish_pid() -> String {
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

fn init_raw_cache_state() -> Option<RawCacheState> {
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let pid = fish_pid();
    let cache_dir = format!("/tmp/fzf-history-{}", user);
    let dirs_file = format!("{}/universal-last-dirs-{}", cache_dir, pid);
    let files_file = format!("{}/universal-last-files-{}", cache_dir, pid);
    fs::create_dir_all(&cache_dir).ok()?;
    let dirs = BufWriter::new(File::create(&dirs_file).ok()?);
    let files = BufWriter::new(File::create(&files_file).ok()?);
    Some(RawCacheState {
        dirs,
        files,
        seen_dirs: HashSet::new(),
        seen_files: HashSet::new(),
    })
}

fn cache_raw_record_path(path: &str, is_dir: bool, state: &mut RawCacheState) {
    if is_dir {
        let mut p = path.to_string();
        if !p.ends_with('/') {
            p.push('/');
        }
        if state.seen_dirs.insert(p.clone()) {
            let _ = writeln!(state.dirs, "{}", p);
        }
    } else {
        if state.seen_files.insert(path.to_string()) {
            let _ = writeln!(state.files, "{}", path);
        }
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

fn snapshot_cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join("unearth").join("snapshots"));
    }
    env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join(".cache")
            .join("unearth")
            .join("snapshots")
    })
}

fn unearth_cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join("unearth"));
    }
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".cache").join("unearth"))
}

fn index_db_path() -> Option<PathBuf> {
    Some(unearth_cache_dir()?.join("index").join("unearth.db"))
}

fn hash_string(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn index_state_path(root_key: &str, suffix: &str) -> Option<PathBuf> {
    Some(unearth_cache_dir()?.join("index").join(format!(
        "{:016x}.{}",
        hash_string(root_key),
        suffix
    )))
}

fn path_age_at_least(path: &Path, min_age: Duration) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    modified.elapsed().map(|age| age >= min_age).unwrap_or(true)
}

fn write_stamp(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = File::create(path);
}

fn open_index_db() -> Result<Connection, String> {
    let path =
        index_db_path().ok_or_else(|| "Could not determine unearth cache dir".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        CREATE TABLE IF NOT EXISTS strings (
            id INTEGER PRIMARY KEY,
            value TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS dirs (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS entries (
            id INTEGER PRIMARY KEY,
            dir_id INTEGER NOT NULL,
            name_id INTEGER NOT NULL,
            kind INTEGER NOT NULL,
            mtime INTEGER,
            size INTEGER,
            UNIQUE(dir_id, name_id, kind)
        );
        CREATE TABLE IF NOT EXISTS indexed_roots (
            root TEXT PRIMARY KEY,
            refreshed_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS index_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS strings_fts USING fts5(
            value,
            content='strings',
            content_rowid='id',
            tokenize='trigram'
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS dirs_fts USING fts5(
            path,
            content='dirs',
            content_rowid='id',
            tokenize='trigram'
        );
        CREATE TRIGGER IF NOT EXISTS strings_fts_ai AFTER INSERT ON strings BEGIN
            INSERT INTO strings_fts(rowid, value) VALUES (new.id, new.value);
        END;
        CREATE TRIGGER IF NOT EXISTS strings_fts_ad AFTER DELETE ON strings BEGIN
            INSERT INTO strings_fts(strings_fts, rowid, value)
            VALUES ('delete', old.id, old.value);
        END;
        CREATE TRIGGER IF NOT EXISTS strings_fts_au AFTER UPDATE ON strings BEGIN
            INSERT INTO strings_fts(strings_fts, rowid, value)
            VALUES ('delete', old.id, old.value);
            INSERT INTO strings_fts(rowid, value) VALUES (new.id, new.value);
        END;
        CREATE TRIGGER IF NOT EXISTS dirs_fts_ai AFTER INSERT ON dirs BEGIN
            INSERT INTO dirs_fts(rowid, path) VALUES (new.id, new.path);
        END;
        CREATE TRIGGER IF NOT EXISTS dirs_fts_ad AFTER DELETE ON dirs BEGIN
            INSERT INTO dirs_fts(dirs_fts, rowid, path)
            VALUES ('delete', old.id, old.path);
        END;
        CREATE TRIGGER IF NOT EXISTS dirs_fts_au AFTER UPDATE ON dirs BEGIN
            INSERT INTO dirs_fts(dirs_fts, rowid, path)
            VALUES ('delete', old.id, old.path);
            INSERT INTO dirs_fts(rowid, path) VALUES (new.id, new.path);
        END;
        CREATE INDEX IF NOT EXISTS idx_strings_value ON strings(value);
        CREATE INDEX IF NOT EXISTS idx_dirs_path ON dirs(path);
        CREATE INDEX IF NOT EXISTS idx_entries_dir ON entries(dir_id);
        CREATE INDEX IF NOT EXISTS idx_entries_name ON entries(name_id);
        ",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn ensure_index_search_ready(conn: &Connection) -> Result<bool, String> {
    let ready: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM index_meta WHERE key = 'fts_trigram_v1'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if ready > 0 {
        return Ok(true);
    }
    conn.execute_batch(
        "
        INSERT INTO strings_fts(strings_fts) VALUES ('rebuild');
        INSERT INTO dirs_fts(dirs_fts) VALUES ('rebuild');
        INSERT OR REPLACE INTO index_meta(key, value)
        VALUES ('fts_trigram_v1', 'ready');
        ",
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}

fn db_get_or_insert_string(tx: &rusqlite::Transaction<'_>, value: &str) -> Result<i64, String> {
    tx.prepare_cached("INSERT OR IGNORE INTO strings(value) VALUES (?)")
        .map_err(|e| e.to_string())?
        .execute([value])
        .map_err(|e| e.to_string())?;
    tx.prepare_cached("SELECT id FROM strings WHERE value = ?")
        .map_err(|e| e.to_string())?
        .query_row([value], |row| row.get(0))
        .map_err(|e| e.to_string())
}

fn db_get_or_insert_dir(tx: &rusqlite::Transaction<'_>, path: &str) -> Result<i64, String> {
    tx.prepare_cached("INSERT OR IGNORE INTO dirs(path) VALUES (?)")
        .map_err(|e| e.to_string())?
        .execute([path])
        .map_err(|e| e.to_string())?;
    tx.prepare_cached("SELECT id FROM dirs WHERE path = ?")
        .map_err(|e| e.to_string())?
        .query_row([path], |row| row.get(0))
        .map_err(|e| e.to_string())
}

fn db_get_or_insert_string_cached(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    value: &str,
) -> Result<i64, String> {
    if let Some(id) = cache.get(value) {
        return Ok(*id);
    }
    let id = db_get_or_insert_string(tx, value)?;
    cache.insert(value.to_string(), id);
    Ok(id)
}

fn db_get_or_insert_dir_cached(
    tx: &rusqlite::Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    path: &str,
) -> Result<i64, String> {
    if let Some(id) = cache.get(path) {
        return Ok(*id);
    }
    let id = db_get_or_insert_dir(tx, path)?;
    cache.insert(path.to_string(), id);
    Ok(id)
}

fn normalize_index_dir(path: &Path) -> String {
    let mut out = path.to_string_lossy().to_string();
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn normalize_index_root_arg(root_raw: &str) -> Result<String, String> {
    let expanded = PathBuf::from(expand_home_path(root_raw));
    if let Ok(canonical) = fs::canonicalize(&expanded) {
        return Ok(normalize_index_dir(&canonical));
    }
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .map_err(|e| e.to_string())?
            .join(expanded)
    };
    Ok(normalize_index_dir(&normalize_lexical_path(&absolute)))
}

fn sql_like_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn sql_like_from_wildcard(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    '%' | '_' | '\\' => {
                        out.push('\\');
                        out.push(next);
                    }
                    '*' => out.push('*'),
                    _ => {
                        out.push('\\');
                        out.push(next);
                    }
                }
            } else {
                out.push('\\');
            }
            continue;
        }
        match ch {
            '*' => out.push('%'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn fts_trigram_query(raw: &str) -> Option<String> {
    if raw.contains('*') || raw.contains('/') || is_wrapped_quote(raw) || raw.chars().count() < 3 {
        return None;
    }
    Some(format!("\"{}\"", raw.replace('"', "\"\"")))
}

fn sql_prefilter_for_term(
    raw: &str,
    regex_mode: bool,
    force_full: bool,
    field_expr: &str,
    fts_ready: bool,
) -> Option<(String, Vec<String>)> {
    if regex_mode {
        return None;
    }

    let mut params = Vec::new();
    if is_wrapped_quote(raw) {
        let mut inner = raw[1..raw.len() - 1].to_string();
        inner = inner.trim_start_matches('/').to_string();
        if inner != "/" {
            inner = inner.trim_end_matches('/').to_string();
        }
        if inner.contains('*') {
            params.push(sql_like_from_wildcard(&inner.to_lowercase()));
            return Some((format!("{} LIKE ? ESCAPE '\\'", field_expr), params));
        }
        params.push(inner.to_lowercase());
        return Some((format!("{} = ?", field_expr), params));
    }

    if force_full {
        if !raw.contains('/') {
            if fts_ready {
                if let Some(query) = fts_trigram_query(raw) {
                    params.push(query.clone());
                    params.push(query);
                    return Some((
                        "(e.name_id IN (SELECT rowid FROM strings_fts WHERE strings_fts MATCH ?) OR e.dir_id IN (SELECT rowid FROM dirs_fts WHERE dirs_fts MATCH ?))".to_string(),
                        params,
                    ));
                }
            }
            let lowered = raw.to_lowercase();
            if raw.contains('*') {
                let like = sql_like_from_wildcard(&lowered);
                params.push(like.clone());
                params.push(like);
                return Some((
                    "(e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\') OR e.dir_id IN (SELECT id FROM dirs WHERE lower(path) LIKE ? ESCAPE '\\'))".to_string(),
                    params,
                ));
            }
            let narrowed = if raw.starts_with('/') && raw.ends_with('/') && raw.len() > 1 {
                lowered[1..lowered.len() - 1].to_string()
            } else if raw.starts_with('/') && raw.len() > 1 {
                lowered.trim_start_matches('/').to_string()
            } else if raw != "/" && raw.ends_with('/') {
                lowered.trim_end_matches('/').to_string()
            } else {
                lowered
            };
            params.push(narrowed.clone());
            params.push(narrowed);
            return Some((
                "(e.name_id IN (SELECT id FROM strings WHERE instr(lower(value), ?) > 0) OR e.dir_id IN (SELECT id FROM dirs WHERE instr(lower(path), ?) > 0))".to_string(),
                params,
            ));
        }

        if raw.contains('*') {
            params.push(sql_like_from_wildcard(&raw.to_lowercase()));
            return Some((format!("{} LIKE ? ESCAPE '\\'", field_expr), params));
        }
        let lowered = raw.to_lowercase();
        if raw.starts_with('/') && raw.ends_with('/') && raw.len() > 1 {
            params.push(lowered[1..lowered.len() - 1].to_string());
        } else if raw.starts_with('/') && raw.len() > 1 {
            params.push(lowered.trim_start_matches('/').to_string());
        } else if raw != "/" && raw.ends_with('/') {
            params.push(lowered.trim_end_matches('/').to_string());
        } else {
            params.push(lowered);
        }
        return Some((format!("instr({}, ?) > 0", field_expr), params));
    }

    if raw.starts_with('/') && raw.ends_with('/') {
        params.push(raw[1..raw.len() - 1].to_lowercase());
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) = ?)".to_string(),
            params,
        ));
    }
    if raw.starts_with('/') {
        params.push(format!("{}%", sql_like_escape(&raw[1..].to_lowercase())));
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\')"
                .to_string(),
            params,
        ));
    }
    if raw != "/" && raw.ends_with('/') {
        params.push(format!(
            "%{}",
            sql_like_escape(&raw[..raw.len() - 1].to_lowercase())
        ));
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\')"
                .to_string(),
            params,
        ));
    }
    if raw.contains('*') {
        params.push(sql_like_from_wildcard(&raw.to_lowercase()));
        return Some((
            "e.name_id IN (SELECT id FROM strings WHERE lower(value) LIKE ? ESCAPE '\\')"
                .to_string(),
            params,
        ));
    }

    if fts_ready {
        if let Some(query) = fts_trigram_query(raw) {
            params.push(query);
            return Some((
                "e.name_id IN (SELECT rowid FROM strings_fts WHERE strings_fts MATCH ?)"
                    .to_string(),
                params,
            ));
        }
    }
    params.push(raw.to_lowercase());
    Some((
        "e.name_id IN (SELECT id FROM strings WHERE instr(lower(value), ?) > 0)".to_string(),
        params,
    ))
}

fn index_path_prefix(path: &str) -> String {
    if path == "/" {
        "/".to_string()
    } else {
        format!("{}/", path.trim_end_matches('/'))
    }
}

fn is_root_index_prune_child(root_key: &str, path: &Path) -> bool {
    root_key == "/" && matches!(path.to_str(), Some("/proc" | "/sys" | "/dev" | "/run"))
}

fn is_root_index_excluded_path(root_key: &str, path: &str) -> bool {
    root_key == "/"
        && ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .any(|prefix| path == *prefix || path.starts_with(&format!("{}/", prefix)))
}

fn purge_index_root(root_raw: &str) -> Result<(), String> {
    let root_key = normalize_index_root_arg(root_raw)?;
    let root_prefix = index_path_prefix(&root_key);
    let mut conn = open_index_db()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut impacted_roots = Vec::<String>::new();
    {
        let mut stmt = tx
            .prepare("SELECT root FROM indexed_roots")
            .map_err(|e| e.to_string())?;
        let roots = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for root in roots {
            let root = root.map_err(|e| e.to_string())?;
            let indexed_prefix = index_path_prefix(&root);
            if root == root_key
                || root.starts_with(&root_prefix)
                || root_key.starts_with(&indexed_prefix)
            {
                impacted_roots.push(root);
            }
        }
    }
    tx.execute(
        "DELETE FROM entries WHERE dir_id IN (
            SELECT id FROM dirs WHERE path = ?1 OR path LIKE ?2
        )",
        params![root_key, format!("{}%", root_prefix)],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM dirs WHERE path = ?1 OR path LIKE ?2",
        params![root_key, format!("{}%", root_prefix)],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM indexed_roots WHERE root = ?1 OR root LIKE ?2",
        params![root_key, format!("{}%", root_prefix)],
    )
    .map_err(|e| e.to_string())?;
    for impacted_root in &impacted_roots {
        tx.execute("DELETE FROM indexed_roots WHERE root = ?1", [impacted_root])
            .map_err(|e| e.to_string())?;
    }
    tx.execute(
        "DELETE FROM strings WHERE id NOT IN (
            SELECT DISTINCT name_id FROM entries
        )",
        [],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    let mut state_roots = impacted_roots;
    state_roots.push(root_key);
    state_roots.sort();
    state_roots.dedup();
    for state_root in state_roots {
        if let Some(stamp_path) = index_state_path(&state_root, "stamp") {
            let _ = fs::remove_file(stamp_path);
        }
        if let Some(lock_path) = index_state_path(&state_root, "lock") {
            let _ = fs::remove_file(lock_path);
        }
    }
    Ok(())
}

fn refresh_index_root(root_raw: &str, opts: &Options) -> Result<(), String> {
    let root = fs::canonicalize(expand_home_path(root_raw)).map_err(|e| e.to_string())?;
    if !root.is_dir() {
        return Err(format!(
            "--index-refresh target '{}' is not a directory",
            root_raw
        ));
    }
    let root_key = normalize_index_dir(&root);
    let root_prefix = index_path_prefix(&root_key);
    let mut conn = open_index_db()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM entries WHERE dir_id IN (
            SELECT id FROM dirs WHERE path = ?1 OR path LIKE ?2
        )",
        params![root_key, format!("{}%", root_prefix)],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM dirs WHERE path = ?1 OR path LIKE ?2",
        params![root_key, format!("{}%", root_prefix)],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM indexed_roots WHERE root = ?1 OR root LIKE ?2",
        params![root_key, format!("{}%", root_prefix)],
    )
    .map_err(|e| e.to_string())?;
    tx.execute_batch(
        "
        DROP INDEX IF EXISTS idx_entries_dir;
        DROP INDEX IF EXISTS idx_entries_name;
        ",
    )
    .map_err(|e| e.to_string())?;
    let mut dir_ids = HashMap::<String, i64>::new();
    let mut string_ids = HashMap::<String, i64>::new();
    db_get_or_insert_dir_cached(&tx, &mut dir_ids, &root_key)?;

    for entry in WalkDir::new(&root)
        .skip_hidden(false)
        .parallelism(Parallelism::RayonNewPool(opts.threads_override))
        .process_read_dir({
            let root_key = root_key.clone();
            move |_depth, _path, _state, children| {
                for child in children.iter_mut() {
                    if let Ok(entry) = child {
                        if let Some(child_path) = entry.read_children_path.as_ref() {
                            if is_root_index_prune_child(&root_key, child_path.as_ref()) {
                                entry.read_children_path = None;
                            }
                        }
                    }
                }
            }
        })
    {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path == root {
            continue;
        }
        let path_key = normalize_index_dir(&path);
        if is_root_index_excluded_path(&root_key, &path_key) {
            continue;
        }
        let file_type = entry.file_type();
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let Some(parent) = path.parent() else {
            continue;
        };
        let parent_key = normalize_index_dir(parent);
        let parent_id = db_get_or_insert_dir_cached(&tx, &mut dir_ids, &parent_key)?;
        let name_id = db_get_or_insert_string_cached(&tx, &mut string_ids, &name)?;
        let kind = if file_type.is_dir() {
            1i64
        } else if file_type.is_symlink() {
            2i64
        } else {
            0i64
        };
        tx.prepare_cached(
            "INSERT OR REPLACE INTO entries(dir_id, name_id, kind, mtime, size)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(|e| e.to_string())?
        .execute(params![
            parent_id,
            name_id,
            kind,
            Option::<i64>::None,
            Option::<i64>::None
        ])
        .map_err(|e| e.to_string())?;
        if kind == 1 {
            db_get_or_insert_dir_cached(&tx, &mut dir_ids, &path_key)?;
        }
    }
    let refreshed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    tx.execute(
        "INSERT OR REPLACE INTO indexed_roots(root, refreshed_at) VALUES (?1, ?2)",
        params![root_key, refreshed_at],
    )
    .map_err(|e| e.to_string())?;
    tx.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_entries_dir ON entries(dir_id);
        CREATE INDEX IF NOT EXISTS idx_entries_name ON entries(name_id);
        ",
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    if let Some(stamp) = index_state_path(&root_key, "stamp") {
        write_stamp(&stamp);
    }
    Ok(())
}

fn index_root_is_known(conn: &Connection, root_key: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM indexed_roots
             WHERE root = ?1
             LIMIT 1",
            params![root_key],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(count > 0)
}

fn spawn_index_refresh(root_key: &str, opts: &Options, force: bool) {
    let Some(stamp_path) = index_state_path(root_key, "stamp") else {
        return;
    };
    if !force && stamp_path.is_file() && !path_age_at_least(&stamp_path, INDEX_REFRESH_MIN_AGE) {
        return;
    }
    let Some(lock_path) = index_state_path(root_key, "lock") else {
        return;
    };
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let lock = File::options()
        .write(true)
        .create_new(true)
        .open(&lock_path);
    if lock.is_err() {
        return;
    }
    let Ok(exe) = env::current_exe() else {
        let _ = fs::remove_file(lock_path);
        return;
    };
    let mut command = Command::new(exe);
    command
        .arg("--index-refresh")
        .arg(root_key)
        .arg("--threads")
        .arg(opts.threads_override.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if command.spawn().is_err() {
        let _ = fs::remove_file(lock_path);
    }
}

fn remove_index_refresh_lock(root_raw: &str) {
    let Ok(root) = fs::canonicalize(expand_home_path(root_raw)) else {
        return;
    };
    let root_key = normalize_index_dir(&root);
    if let Some(lock_path) = index_state_path(&root_key, "lock") {
        let _ = fs::remove_file(lock_path);
    }
}

fn path_has_hidden_component(path: &str) -> bool {
    path.split('/')
        .any(|part| part.len() > 1 && part.starts_with('.'))
}

fn indexed_root_from_opts(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
) -> Result<(PathBuf, Vec<String>), String> {
    if let Some(spec) = content_spec {
        return Ok((spec.root.clone(), spec.terms.clone()));
    }
    let mut terms = opts.positional.clone();
    let root = if terms.len() > 1 {
        let last = terms.last().cloned().unwrap();
        if is_implicit_content_path_token(&last) || Path::new(&expand_home_path(&last)).is_dir() {
            terms.pop();
            PathBuf::from(expand_home_path(&last))
        } else {
            PathBuf::from(".")
        }
    } else {
        PathBuf::from(".")
    };
    if terms.is_empty() {
        return Err("--index requires at least one search term".to_string());
    }
    Ok((root, terms))
}

fn run_indexed(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let (root_raw, terms) = indexed_root_from_opts(opts, content_spec)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let root_prefix = index_path_prefix(&root_key);
    let mut type_flag = if opts.force_dir {
        Some(TypeFlag::Dir)
    } else if opts.force_file {
        Some(TypeFlag::File)
    } else {
        None
    };
    let mut regexes = Vec::new();
    for term in &terms {
        let parsed = parse_name_pattern(term, opts.regex_mode);
        if parsed.type_flag == Some(TypeFlag::Dir) && !opts.force_file {
            type_flag = Some(TypeFlag::Dir);
        }
        regexes.push(
            RegexBuilder::new(&parsed.regex)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))?,
        );
    }
    let conn = open_index_db()?;
    let fts_ready = ensure_index_search_ready(&conn).unwrap_or(false);
    let root_known = index_root_is_known(&conn, &root_key)?;
    spawn_index_refresh(&root_key, opts, !root_known);
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let base_name_expr = "lower(s.value)";
    let full_path_expr =
        "lower((CASE WHEN d.path = '/' THEN '/' || s.value ELSE d.path || '/' || s.value END) || CASE WHEN e.kind = 1 THEN '/' ELSE '' END)";
    let sql_field_expr = if opts.force_full {
        full_path_expr
    } else {
        base_name_expr
    };
    let mut sql = String::from(
        "SELECT d.path, s.value, e.kind, e.mtime, e.size
         FROM entries e
         JOIN dirs d ON e.dir_id = d.id
         JOIN strings s ON e.name_id = s.id
         WHERE (d.path = ? OR d.path LIKE ?)",
    );
    let mut sql_params = vec![root_key.clone(), format!("{}%", root_prefix)];
    match type_flag {
        Some(TypeFlag::Dir) => sql.push_str(" AND e.kind = 1"),
        Some(TypeFlag::File) => sql.push_str(" AND e.kind != 1"),
        None => {}
    }
    for term in &terms {
        if let Some((condition, extra_params)) = sql_prefilter_for_term(
            term,
            opts.regex_mode,
            opts.force_full,
            sql_field_expr,
            fts_ready,
        ) {
            sql.push_str(" AND ");
            sql.push('(');
            sql.push_str(&condition);
            sql.push(')');
            sql_params.extend(extra_params);
        }
    }
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params_from_iter(sql_params.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    if can_stream_direct(opts, use_style) {
        let stdout = io::stdout();
        let mut lock = BufWriter::with_capacity(64 * 1024, stdout.lock());
        let mut cache_state = if opts.cache_output {
            init_raw_cache_state()
        } else {
            None
        };
        let mut written_since_flush = 0usize;

        for row in rows {
            let (dir_path, name, kind) = row.map_err(|e| e.to_string())?;
            let is_dir = kind == 1;
            if matches!(type_flag, Some(TypeFlag::Dir)) && !is_dir {
                continue;
            }
            if matches!(type_flag, Some(TypeFlag::File)) && is_dir {
                continue;
            }
            let mut path = if dir_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", dir_path, name)
            };
            if is_dir {
                path.push('/');
            }
            if is_root_index_excluded_path(&root_key, &path) {
                continue;
            }
            if opts.visible_only && path_has_hidden_component(&path) {
                continue;
            }
            let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            let matched = if opts.force_full {
                regexes.iter().all(|re| re.is_match(&path))
            } else {
                regexes.iter().all(|re| re.is_match(base))
            };
            if !matched {
                continue;
            }
            if opts.force_full && regexes.len() > 1 && !regexes.iter().any(|re| re.is_match(base)) {
                continue;
            }
            if let Some(state) = cache_state.as_mut() {
                cache_raw_record_path(&path, is_dir, state);
            }
            if opts.index_binary {
                let _ = write_binary_path_record(&mut lock, &path);
            } else {
                let _ = lock.write_all(path.as_bytes());
                let _ = lock.write_all(b"\n");
            }
            written_since_flush += 1;
            if written_since_flush >= INDEX_STREAM_FLUSH_LINES {
                let _ = lock.flush();
                written_since_flush = 0;
            }
        }
        let _ = lock.flush();
        if let Some(mut state) = cache_state {
            let _ = state.dirs.flush();
            let _ = state.files.flush();
        }
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: false,
        });
    }

    let mut results = Vec::new();
    for row in rows {
        let (dir_path, name, kind) = row.map_err(|e| e.to_string())?;
        let is_dir = kind == 1;
        let is_symlink = kind == 2;
        if matches!(type_flag, Some(TypeFlag::Dir)) && !is_dir {
            continue;
        }
        if matches!(type_flag, Some(TypeFlag::File)) && is_dir {
            continue;
        }
        let mut path = if dir_path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", dir_path, name)
        };
        if is_dir {
            path.push('/');
        }
        if is_root_index_excluded_path(&root_key, &path) {
            continue;
        }
        if opts.visible_only && path_has_hidden_component(&path) {
            continue;
        }
        let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let matched = if opts.force_full {
            regexes.iter().all(|re| re.is_match(&path))
        } else {
            regexes.iter().all(|re| re.is_match(base))
        };
        if !matched {
            continue;
        }
        if opts.force_full && regexes.len() > 1 && !regexes.iter().any(|re| re.is_match(base)) {
            continue;
        }
        results.push(SearchResult {
            path,
            is_dir,
            is_symlink,
            metadata: None,
        });
    }
    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(SearchRun {
        lines: final_transform(results, opts, use_style, stdout_is_tty, colors, cache, None),
        timed_out: false,
    })
}

fn snapshot_args_key_parts() -> Vec<String> {
    env::args()
        .skip(1)
        .filter(|arg| arg != "--snapshot-cache" && arg != "--snapshot-refresh")
        .collect()
}

fn snapshot_cache_path() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    for arg in snapshot_args_key_parts() {
        arg.hash(&mut hasher);
    }
    Some(snapshot_cache_dir()?.join(format!("{:016x}.paths", hasher.finish())))
}

fn snapshot_lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

fn snapshot_is_stale(path: &Path) -> bool {
    path_age_at_least(path, SNAPSHOT_REFRESH_MIN_AGE)
}

fn stream_snapshot_cache(path: &Path) -> io::Result<()> {
    let mut input = File::open(path)?;
    let stdout = io::stdout();
    let mut output = BufWriter::with_capacity(128 * 1024, stdout.lock());
    io::copy(&mut input, &mut output)?;
    output.flush()
}

fn write_snapshot_cache(path: &Path, lines: &[String]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let file = File::create(&tmp)?;
        let mut writer = BufWriter::with_capacity(128 * 1024, file);
        for line in lines {
            writeln!(writer, "{}", line)?;
        }
        writer.flush()?;
    }
    fs::rename(tmp, path)
}

fn spawn_snapshot_refresh() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(cache_path) = snapshot_cache_path() else {
        return;
    };
    if cache_path.is_file() && !snapshot_is_stale(&cache_path) {
        return;
    }
    let lock_path = snapshot_lock_path(&cache_path);
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let lock = File::options()
        .write(true)
        .create_new(true)
        .open(&lock_path);
    if lock.is_err() {
        return;
    }
    let mut args = snapshot_args_key_parts();
    args.push("--snapshot-refresh".to_string());
    if Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_err()
    {
        let _ = fs::remove_file(lock_path);
    }
}

fn cache_transform(items: &Vec<SearchResult>, opts: &Options) {
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

fn get_dirsize_stats(path: &str, cache: &mut DirStatsCache) -> Option<DirStats> {
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

fn get_dirsize_bytes(path: &str, cache: &mut DirStatsCache) -> Option<u64> {
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

fn add_info_transform(
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
    let mut out = Vec::new();
    for item in items {
        if let Some(meta) = &item.metadata {
            let dt: DateTime<Local> = meta
                .modified()
                .unwrap_or_else(|_| std::time::SystemTime::now())
                .into();
            let dt_str = dt.format("%Y-%m-%d %H:%M:%S").to_string();
            let mut human_size = format_size_iec(meta.len());
            let mut extra = String::new();
            if opts.long_extended {
                if item.is_symlink {
                    let link_path = item.path.trim_end_matches('/');
                    if fs::metadata(link_path).map(|m| m.is_dir()).unwrap_or(false) {
                        human_size = format_size_iec(meta.len());
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
            out.push(format!(
                "{} {}{} {}",
                dt_str, human_size, extra, path_display
            ));
        } else {
            out.push(item.path);
        }
    }
    out
}

fn sizes_transform(
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
        out.push(format!("{}\t{}", compact, path_display));
    }
    out
}

fn counts_summary_transform(
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
        };
        let folder_display =
            render_styled_path(&folder_item, use_style, false, colors, opts, highlight);
        out.push(format!("{:>7}  {}", n, folder_display));
    }
    out
}

fn parse_ls_colors() -> ColorSpec {
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

fn default_color_spec() -> ColorSpec {
    ColorSpec {
        by_key: HashMap::new(),
        globs: Vec::new(),
        color_prefix_dir: "38;2;255;255;255".to_string(),
        color_dir: "01;34".to_string(),
        color_link: "01;36".to_string(),
        color_exec: "01;32".to_string(),
    }
}

fn color_code_for_path(res: &SearchResult, colors: &ColorSpec) -> String {
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

fn decorator_for_res(res: &SearchResult) -> Option<char> {
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

const MATCH_HIGHLIGHT_CODE: &str = "1;91";

fn compile_highlight_spec(patterns: &[(String, bool)]) -> Result<HighlightSpec, String> {
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

fn merge_match_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
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

fn colorize_segment_with_highlights(
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

fn render_styled_path(
    res: &SearchResult,
    use_style: bool,
    add_decorator: bool,
    colors: &ColorSpec,
    opts: &Options,
    highlight: Option<&HighlightSpec>,
) -> String {
    let mut display_path = res.path.clone();
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
        if let Some(spec) = highlight {
            let mut plain = String::new();
            if !prefix.is_empty() {
                plain.push_str(&colorize_segment_with_highlights(
                    &prefix,
                    None,
                    &spec.prefix_rules,
                ));
            }
            plain.push_str(&colorize_segment_with_highlights(
                &leaf,
                None,
                &spec.leaf_rules,
            ));
            return plain;
        }
        return display_path;
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
        let mut abs_prefix = prefix.clone();
        if !abs_prefix.is_empty() && !abs_prefix.starts_with('/') {
            if let Ok(cwd) = env::current_dir() {
                abs_prefix = format!("{}/{}", cwd.display(), abs_prefix.trim_start_matches("./"));
            }
        }
        let mut abs_leaf = res.path.clone();
        if !abs_leaf.starts_with('/') {
            if let Ok(cwd) = env::current_dir() {
                abs_leaf = format!("{}/{}", cwd.display(), abs_leaf.trim_start_matches("./"));
            }
        }
        if prefix.is_empty() {
            final_str = format!(
                "\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\",
                abs_leaf, final_str
            );
        } else {
            final_str = format!(
                "\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\",
                abs_prefix, prefix_colored, abs_leaf, leaf_colored
            );
        }
    }
    final_str
}

fn final_transform(
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
        return counts_summary_transform(items, stdout_is_tty, use_style, colors, opts, highlight);
    }
    precompute_dirsize_cache(&items, opts, cache);
    let items = sort_results(items, opts, cache);
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

fn run_standard(
    opts: &Options,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let name = parse_name_pattern(&opts.positional[0], opts.regex_mode);
    let mut type_flag = name.type_flag;
    if opts.force_dir {
        type_flag = Some(TypeFlag::Dir);
    }
    if opts.force_file {
        type_flag = Some(TypeFlag::File);
    }
    let stream_direct = can_stream_direct(opts, use_style);
    let re = RegexBuilder::new(&name.regex)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("Invalid regex: {}", e))?;
    let is_catch_all = name.regex == ".*" || name.regex == "^.*$";
    let timeout_dur = opts.timeout_dur;
    let timeout_triggered = Arc::new(AtomicBool::new(false));
    let timeout_clone = timeout_triggered.clone();
    std::thread::spawn(move || {
        std::thread::sleep(timeout_dur);
        timeout_clone.store(true, Ordering::Relaxed);
    });

    if stream_direct {
        let (tx, rx) = unbounded::<Vec<PathInfo>>();
        let opts_clone = opts.clone();
        let timeout_fast = timeout_triggered.clone();
        if opts.positional.len() == 1 {
            rayon::spawn(move || {
                walk_fast(
                    PathBuf::from("."),
                    &re,
                    is_catch_all,
                    &tx,
                    opts_clone.visible_only,
                    opts_clone.respect_ignore,
                    opts_clone.no_recurse,
                    opts_clone.follow_links,
                    type_flag,
                    false,
                    false,
                    &timeout_fast,
                )
            });
        } else {
            let p_raw = &opts.positional[1];
            let sd = parse_search_dir(p_raw, opts.regex_mode, opts.force_pattern_mode);
            rayon::spawn(move || match sd {
                SearchDirMode::Path(p) => {
                    let root_serial = root_prefers_single_thread(Path::new(&p));
                    walk_fast(
                        PathBuf::from(p),
                        &re,
                        is_catch_all,
                        &tx,
                        opts_clone.visible_only,
                        opts_clone.respect_ignore,
                        opts_clone.no_recurse,
                        opts_clone.follow_links,
                        type_flag,
                        false,
                        root_serial,
                        &timeout_fast,
                    )
                }
                SearchDirMode::Pattern(sd_rx) => {
                    let mut roots = Vec::new();
                    let (rtx, rrx) = unbounded::<Vec<PathInfo>>();
                    let sd_re = RegexBuilder::new(&sd_rx)
                        .case_insensitive(true)
                        .build()
                        .unwrap();
                    walk_fast(
                        PathBuf::from("/"),
                        &sd_re,
                        false,
                        &rtx,
                        opts_clone.visible_only,
                        opts_clone.respect_ignore,
                        false,
                        opts_clone.follow_links,
                        Some(TypeFlag::Dir),
                        false,
                        false,
                        &timeout_fast,
                    );
                    drop(rtx);
                    for chunk in rrx {
                        for info in chunk {
                            roots.push(info.path);
                        }
                    }
                    roots.into_par_iter().for_each_with(tx.clone(), |tx_c, d| {
                        let next_serial = root_prefers_single_thread(&d);
                        walk_fast(
                            d,
                            &re,
                            is_catch_all,
                            tx_c,
                            opts_clone.visible_only,
                            opts_clone.respect_ignore,
                            opts_clone.no_recurse,
                            opts_clone.follow_links,
                            type_flag,
                            false,
                            next_serial,
                            &timeout_fast,
                        );
                    });
                }
            });
        }

        let stdout = io::stdout();
        let mut lock = BufWriter::with_capacity(128 * 1024, stdout.lock());
        let mut cache_state = if opts.cache_output {
            init_raw_cache_state()
        } else {
            None
        };

        for chunk in rx {
            for info in chunk {
                if let Some(state) = cache_state.as_mut() {
                    cache_raw_record_path(&info.path.to_string_lossy(), info.is_dir, state);
                }
                let _ = lock.write_all(info.path.as_os_str().as_bytes());
                if info.is_dir && !info.path.as_os_str().as_bytes().ends_with(b"/") {
                    let _ = lock.write_all(b"/");
                }
                let _ = lock.write_all(b"\n");
            }
        }

        if let Some(mut state) = cache_state {
            let _ = state.dirs.flush();
            let _ = state.files.flush();
        }
        let _ = lock.flush();
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: timeout_triggered.load(Ordering::Relaxed),
        });
    }

    let mut results = Vec::new();
    let needs_metadata = opts.long_format || opts.sort_field.is_some() || opts.sizes;
    let (tx, rx) = unbounded::<Vec<SearchResult>>();
    let opts_clone = opts.clone();
    if opts.positional.len() == 1 {
        let timeout_walk = timeout_triggered.clone();
        rayon::spawn(move || {
            walk_rayon_worker(
                PathBuf::from("."),
                &re,
                is_catch_all,
                &tx,
                &opts_clone,
                type_flag,
                false,
                false,
                needs_metadata,
                false,
                &timeout_walk,
            )
        });
    } else {
        let p_raw = &opts.positional[1];
        match parse_search_dir(p_raw, opts.regex_mode, opts.force_pattern_mode) {
            SearchDirMode::Path(p) => {
                let timeout_walk = timeout_triggered.clone();
                rayon::spawn(move || {
                    let root_serial = root_prefers_single_thread(Path::new(&p));
                    walk_rayon_worker(
                        PathBuf::from(p),
                        &re,
                        is_catch_all,
                        &tx,
                        &opts_clone,
                        type_flag,
                        false,
                        false,
                        needs_metadata,
                        root_serial,
                        &timeout_walk,
                    )
                })
            }
            SearchDirMode::Pattern(sd_rx) => {
                let timeout_walk = timeout_triggered.clone();
                rayon::spawn(move || {
                    let mut roots = Vec::new();
                    let (rtx, rrx) = unbounded::<Vec<SearchResult>>();
                    let sd_re = RegexBuilder::new(&sd_rx)
                        .case_insensitive(true)
                        .build()
                        .unwrap();
                    walk_rayon_worker(
                        PathBuf::from("/"),
                        &sd_re,
                        false,
                        &rtx,
                        &opts_clone,
                        Some(TypeFlag::Dir),
                        false,
                        false,
                        false,
                        false,
                        &timeout_walk,
                    );
                    drop(rtx);
                    for chunk in rrx {
                        for r in chunk {
                            roots.push(PathBuf::from(r.path));
                        }
                    }
                    roots.into_par_iter().for_each_with(tx.clone(), |tx_c, d| {
                        let next_serial = root_prefers_single_thread(&d);
                        walk_rayon_worker(
                            d,
                            &re,
                            is_catch_all,
                            tx_c,
                            &opts_clone,
                            type_flag,
                            false,
                            false,
                            needs_metadata,
                            next_serial,
                            &timeout_walk,
                        );
                    });
                });
            }
        }
    }
    for chunk in rx {
        results.extend(chunk);
    }
    let highlight_spec = if opts.highlight_match {
        Some(compile_highlight_spec(&[(name.regex.clone(), false)])?)
    } else {
        None
    };
    Ok(SearchRun {
        lines: final_transform(
            results,
            opts,
            use_style,
            stdout_is_tty,
            colors,
            cache,
            highlight_spec.as_ref(),
        ),
        timed_out: timeout_triggered.load(Ordering::Relaxed),
    })
}

fn run_contains_all(
    opts: &Options,
    spec: ContainsAllSpec,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let timeout_dur = opts.timeout_dur;
    let timeout_triggered = Arc::new(AtomicBool::new(false));
    let timeout_clone = timeout_triggered.clone();
    std::thread::spawn(move || {
        std::thread::sleep(timeout_dur);
        timeout_clone.store(true, Ordering::Relaxed);
    });

    let mut type_flag = if opts.force_dir {
        Some(TypeFlag::Dir)
    } else if opts.force_file {
        Some(TypeFlag::File)
    } else {
        None
    };
    let mut term_specs: Vec<(String, i64)> = Vec::new();
    for p in &spec.terms {
        let parsed = parse_name_pattern(p, opts.regex_mode);
        if parsed.type_flag == Some(TypeFlag::Dir) && !opts.force_file {
            type_flag = Some(TypeFlag::Dir);
        }
        term_specs.push((parsed.regex, term_selectivity_score(p, opts.regex_mode)));
    }
    term_specs.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)))
    });
    let regexes: Vec<String> = term_specs.into_iter().map(|(rx, _)| rx).collect();
    if regexes.is_empty() {
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: false,
        });
    }
    let first_re = RegexBuilder::new(&regexes[0])
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("Invalid regex: {}", e))?;
    let is_catch_all = regexes[0] == ".*" || regexes[0] == "^.*$";
    let needs_metadata = opts.long_format || opts.sort_field.is_some() || opts.sizes;
    let (tx, rx) = unbounded::<Vec<SearchResult>>();
    let opts_clone = opts.clone();
    let root = spec.root.clone();
    let root_serial = root_prefers_single_thread(&root);
    let timeout_walk = timeout_triggered.clone();
    rayon::spawn(move || {
        walk_rayon_worker(
            root,
            &first_re,
            is_catch_all,
            &tx,
            &opts_clone,
            type_flag,
            opts_clone.force_full,
            false,
            needs_metadata,
            root_serial,
            &timeout_walk,
        )
    });

    let mut rows = Vec::new();
    for chunk in rx {
        rows.extend(chunk);
    }
    for rx_str in regexes.iter().skip(1) {
        let re_extra = RegexBuilder::new(rx_str)
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("Invalid regex: {}", e))?;
        rows = rows
            .into_iter()
            .filter(|r| {
                if opts.force_full {
                    re_extra.is_match(&r.path)
                } else {
                    let base = r
                        .path
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("");
                    re_extra.is_match(base)
                }
            })
            .collect();
    }
    if opts.force_full && regexes.len() > 1 {
        let basename_res: Vec<Regex> = regexes
            .iter()
            .map(|rx| {
                RegexBuilder::new(rx)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| format!("Invalid regex: {}", e))
            })
            .collect::<Result<Vec<_>, _>>()?;
        rows = rows
            .into_iter()
            .filter(|r| {
                let base = r
                    .path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("");
                basename_res.iter().any(|re| re.is_match(base))
            })
            .collect();
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    let highlight_spec = if opts.highlight_match {
        let highlight_patterns: Vec<(String, bool)> = regexes
            .iter()
            .cloned()
            .map(|rx| (rx, opts.force_full))
            .collect();
        Some(compile_highlight_spec(&highlight_patterns)?)
    } else {
        None
    };

    Ok(SearchRun {
        lines: final_transform(
            rows,
            opts,
            use_style,
            stdout_is_tty,
            colors,
            cache,
            highlight_spec.as_ref(),
        ),
        timed_out: timeout_triggered.load(Ordering::Relaxed),
    })
}

fn run_full(
    opts: &Options,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let mut search_root = ".".to_string();
    let mut patterns = opts.positional.clone();
    if opts.positional.len() > 1 {
        if let Some(last) = opts.positional.last() {
            if Path::new(last).is_dir() {
                search_root = last.clone();
                patterns.pop();
            }
        }
    }
    let mut type_flag = if opts.force_dir {
        Some(TypeFlag::Dir)
    } else if opts.force_file {
        Some(TypeFlag::File)
    } else {
        None
    };
    let mut pattern_specs: Vec<(String, bool)> = Vec::new();
    for p in &patterns {
        let parsed = parse_name_pattern(p, opts.regex_mode);
        if parsed.type_flag == Some(TypeFlag::Dir) && !opts.force_file {
            type_flag = Some(TypeFlag::Dir);
        }
        pattern_specs.push((parsed.regex, pattern_prefers_full_path(p, opts.regex_mode)));
    }
    if pattern_specs.is_empty() {
        return Ok(SearchRun {
            lines: Vec::new(),
            timed_out: false,
        });
    }
    let re = RegexBuilder::new(&pattern_specs[0].0)
        .case_insensitive(true)
        .build()
        .unwrap();
    let timeout_dur = opts.timeout_dur;
    let timeout_triggered = Arc::new(AtomicBool::new(false));
    let timeout_clone = timeout_triggered.clone();
    std::thread::spawn(move || {
        std::thread::sleep(timeout_dur);
        timeout_clone.store(true, Ordering::Relaxed);
    });
    let (tx, rx) = unbounded::<Vec<SearchResult>>();
    let opts_clone = opts.clone();
    let is_catch_all = pattern_specs[0].0 == ".*" || pattern_specs[0].0 == "^.*$";
    let first_full_path_match = pattern_specs[0].1;
    let prune_matched_dir_subtrees = false;
    let root_serial = root_prefers_single_thread(Path::new(&search_root));
    let timeout_walk = timeout_triggered.clone();
    rayon::spawn(move || {
        walk_rayon_worker(
            PathBuf::from(search_root),
            &re,
            is_catch_all,
            &tx,
            &opts_clone,
            type_flag,
            first_full_path_match,
            prune_matched_dir_subtrees,
            opts_clone.long_format || opts_clone.sort_field.is_some() || opts_clone.sizes,
            root_serial,
            &timeout_walk,
        );
    });
    let mut rows = Vec::new();
    for chunk in rx {
        rows.extend(chunk);
    }
    for (rx_str, full_path_match) in pattern_specs.iter().skip(1) {
        let re_extra = RegexBuilder::new(rx_str)
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("Invalid regex: {}", e))?;
        rows = rows
            .into_iter()
            .filter(|r| {
                if *full_path_match {
                    re_extra.is_match(&r.path)
                } else {
                    let base = r
                        .path
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("");
                    re_extra.is_match(base)
                }
            })
            .collect();
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    let highlight_spec = if opts.highlight_match {
        Some(compile_highlight_spec(&pattern_specs)?)
    } else {
        None
    };
    Ok(SearchRun {
        lines: final_transform(
            rows,
            opts,
            use_style,
            stdout_is_tty,
            colors,
            cache,
            highlight_spec.as_ref(),
        ),
        timed_out: timeout_triggered.load(Ordering::Relaxed),
    })
}

fn main() -> ExitCode {
    let mut opts = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{}", e);
            }
            return ExitCode::from(2);
        }
    };
    if opts.snapshot_refresh && !opts.timeout_explicit {
        opts.timeout_dur = SNAPSHOT_REFRESH_TIMEOUT;
    }
    if let Some(root) = opts.index_refresh.as_deref() {
        let result = refresh_index_root(root, &opts);
        remove_index_refresh_lock(root);
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        };
    }
    if let Some(root) = opts.index_purge.as_deref() {
        return match purge_index_root(root) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        };
    }
    let snapshot_path = if opts.snapshot_cache || opts.snapshot_refresh {
        snapshot_cache_path()
    } else {
        None
    };
    let content_spec = match contains_all_spec_from_opts(&opts) {
        Ok(v) => v,
        Err(e) => {
            if opts.snapshot_refresh {
                if let Some(path) = snapshot_path.as_deref() {
                    let _ = fs::remove_file(snapshot_lock_path(path));
                }
            }
            if !e.trim().is_empty() {
                eprintln!("{}", e.trim());
            }
            return ExitCode::from(2);
        }
    };
    if opts.snapshot_cache {
        if let Some(path) = snapshot_path.as_deref() {
            if path.is_file() {
                if let Err(e) = stream_snapshot_cache(path) {
                    eprintln!("unearth: failed to read snapshot cache: {}", e);
                    return ExitCode::from(1);
                }
                spawn_snapshot_refresh();
                return ExitCode::SUCCESS;
            }
        }
    }
    let effective_threads = effective_threads_override(&opts, content_spec.as_ref());
    let _ = ThreadPoolBuilder::new()
        .num_threads(effective_threads)
        .build_global();
    let mut cache = DirStatsCache {
        map: HashMap::new(),
        bytes_map: HashMap::new(),
    };
    let colors = if opts.color_when == ColorWhen::Never {
        default_color_spec()
    } else {
        parse_ls_colors()
    };
    let result = if opts.index_mode {
        run_indexed(&opts, content_spec.as_ref(), &mut cache, &colors)
    } else if let Some(spec) = content_spec {
        run_contains_all(&opts, spec, &mut cache, &colors)
    } else if opts.force_full {
        run_full(&opts, &mut cache, &colors)
    } else {
        run_standard(&opts, &mut cache, &colors)
    };
    match result {
        Ok(run) => {
            if (opts.snapshot_cache || opts.snapshot_refresh) && !run.timed_out {
                if let Some(path) = snapshot_path.as_deref() {
                    if let Err(e) = write_snapshot_cache(path, &run.lines) {
                        eprintln!("unearth: failed to write snapshot cache: {}", e);
                    }
                }
            }
            if opts.snapshot_refresh {
                if let Some(path) = snapshot_path.as_deref() {
                    let _ = fs::remove_file(snapshot_lock_path(path));
                }
                return if run.timed_out {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                };
            }
            if !run.lines.is_empty() {
                let stdout = io::stdout();
                let mut lock = BufWriter::with_capacity(128 * 1024, stdout.lock());
                for line in run.lines {
                    let _ = writeln!(lock, "{}", line);
                }
                let _ = lock.flush();
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if !e.trim().is_empty() {
                eprintln!("{}", e.trim());
            }
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_opts() -> Options {
        Options {
            timeout_dur: Duration::from_secs(6),
            timeout_explicit: false,
            force_pattern_mode: false,
            long_format: false,
            long_extended: false,
            sizes: false,
            counts: false,
            regex_mode: false,
            sort_field: None,
            sort_order: None,
            no_recurse: false,
            follow_links: false,
            respect_ignore: false,
            visible_only: true,
            threads_override: 8,
            threads_explicit: false,
            cache_output: false,
            snapshot_cache: false,
            snapshot_refresh: false,
            index_mode: false,
            index_binary: false,
            index_refresh: None,
            index_purge: None,
            absolute_paths: false,
            force_dir: false,
            force_file: false,
            force_full: false,
            classify: false,
            color_when: ColorWhen::Auto,
            hyperlinks: false,
            highlight_match: false,
            contains_all: false,
            path_override: None,
            positional: Vec::new(),
        }
    }

    #[test]
    fn media_root_prefers_single_thread() {
        assert!(root_prefers_single_thread(Path::new("/media")));
        assert!(root_prefers_single_thread(Path::new("/media/disk")));
        assert!(!root_prefers_single_thread(Path::new("/mnt")));
        assert!(!root_prefers_single_thread(Path::new("/home/lewis")));
    }

    #[test]
    fn explicit_threads_override_media_default() {
        let mut opts = base_opts();
        opts.threads_override = 32;
        opts.threads_explicit = true;
        let spec = ContainsAllSpec {
            terms: vec!["x".to_string()],
            root: PathBuf::from("/media/disk"),
        };
        assert_eq!(effective_threads_override(&opts, Some(&spec)), 32);
    }

    #[test]
    fn root_index_excludes_volatile_system_trees() {
        assert!(is_root_index_excluded_path("/", "/dev"));
        assert!(is_root_index_excluded_path("/", "/dev/null"));
        assert!(is_root_index_excluded_path("/", "/proc/1/status"));
        assert!(is_root_index_excluded_path("/", "/sys/class"));
        assert!(is_root_index_excluded_path("/", "/run/user"));
        assert!(!is_root_index_excluded_path("/", "/media"));
        assert!(!is_root_index_excluded_path("/", "/home/lewis"));
        assert!(!is_root_index_excluded_path("/dev", "/dev/null"));
    }
}
