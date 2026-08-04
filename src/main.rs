use chrono::{DateTime, Local};
use jwalk::{Parallelism, WalkDir};
use memmap2::Mmap;
use regex::{Regex, RegexBuilder};
use rusqlite::{params, params_from_iter, Connection};
use std::borrow::Cow;
use std::cmp::Ordering as CmpOrdering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, BufWriter, IsTerminal, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{unbounded, Sender};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

mod watcher;

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
const INDEX_INSERT_BATCH_SIZE: usize = 1_000;
const INDEX_INCREMENTAL_MAX_CHANGES: usize = 100_000;
const INDEX_INCREMENTAL_CHANGE_DIVISOR: usize = 5;
const INDEX_SNAPSHOT_MAGIC: &[u8; 8] = b"UNRTHS01";
const INDEX_MANIFEST_MAGIC: &[u8; 8] = b"UNRMNF02";
const INDEX_DELTA_MAGIC: &[u8; 8] = b"UNRDLT01";
const INDEX_DELTA_COMPACT_RECORDS: usize = 50_000;

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
    limit: Option<usize>,
    reverse: bool,
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
    index_if_watched: bool,
    index_binary: bool,
    recent_limit: Option<usize>,
    index_refresh: Option<String>,
    index_snapshot: Option<String>,
    index_purge: Option<String>,
    watch: bool,
    watch_status: bool,
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
    indexed_activity_nanos: Option<i64>,
    indexed_size: Option<u64>,
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
                       [--timeout N] [--sort date|size|name asc|desc] [--limit N]
                       [--reverse]
                       [--no-recurse|-R] [--follow-links]
                       [--ignore] [--hidden|-H] [--threads N]
                       [--cache-raw] [--snapshot-cache]
                       [--index|--index-if-watched] [--index-binary]
                       [--recent N]
                       [--index-refresh DIR] [--index-snapshot DIR]
                       [--index-purge DIR]
                       [--watch ROOT ...] [--watch-status]
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
  - --hyperlink emits split file:// hyperlinks. The parent-directory link
    includes ?select= so PCManFM can preselect the matching file or directory.
  - --counts prints match counts grouped by parent folder.
    This mode outputs summary rows instead of full match paths.
  - --snapshot-cache prints the last complete snapshot for this exact command
    immediately, then refreshes that snapshot in the background. Timed-out
    scans do not replace an existing snapshot. Background refreshes use a long
    timeout by default unless --timeout is passed explicitly.
  - --index queries the global pooled path database in
    ~/.cache/unearth/index/unearth.db instead of walking the filesystem. It
    returns current DB rows immediately and starts one background refresh when
    the root is missing or stale, unless a clean --watch owner covers it.
  - --index-if-watched queries that database only when a clean live watcher
    covers the requested search root. Otherwise it performs the normal live
    filesystem scan. This is intended for shell functions that need an
    automatic indexed-or-scan fallback.
  - --index-binary writes --index results as repeated little-endian
    u32-length-prefixed path bytes instead of newline-delimited text.
  - --recent N queries the indexed database for the N most recently
    created-or-modified entries under DIR (or '.' when DIR is omitted), ordered
    newest-first. It synchronously refreshes the covering indexed root before
    querying unless a clean --watch owner covers it, so results are never
    intentionally taken from a stale snapshot. Filtering is performed in SQL
    before the result metadata is rendered. Files,
    directories, and symlinks are included by default; use -f or -d to restrict
    the type. Terms before DIR filter results; --full/-F matches them against the
    complete path. Add --long/-l to show the activity date and
    size.
  - --index-refresh DIR rebuilds the indexed rows for DIR in that global
    database and atomically replaces its fast-start binary snapshot. Directory
    paths and repeated entry names are stored once and entries link to them by
    integer IDs. Unchanged path/kind fingerprints skip database and snapshot
    reconstruction. Small changes are merge-diffed through a memory-mapped
    manifest and published as snapshot deltas; large changes compact fresh base
    files using in-memory ID assignment and batched full reconstruction.
  - --index-snapshot DIR rebuilds only the fast-start binary snapshot from
    existing indexed rows without walking the filesystem.
  - --index-purge DIR removes indexed rows for DIR and all indexed children.
    Existing roots are canonicalized; missing roots are normalized lexically.
  - --watch ROOT ... starts the live index owner for one or more directory roots. It performs
    an initial scan, then batches create/modify/delete/rename events into the same pooled SQLite
    database. fanotify is preferred when the kernel and permissions support filesystem file
    handles; inotify is used as the recursive fallback. The owner is exclusive per root and
    records a boot ID, process start time, and heartbeat. Queue overflow, unmounts, unsupported
    event resolution, and new mount points trigger a scoped reconciliation scan instead of
    silently losing entries. Periodic safety scans are disabled by default; set
    UNEARTH_WATCH_RECONCILE_SECS to a positive number to enable them. This command stays in
    the foreground until interrupted.
  - --watch-status prints live watcher state recorded in the pooled database and exits.
    It marks a state stopped when the recorded watcher process is no longer alive.
  - --sizes prints compact sizes as SIZE<TAB>PATH (max 6 chars including
    unit, e.g., 1.111M, 111.1M),
    using recursive directory totals for directory matches.
    For top-level system trees under / (/mnt, /media, /dev, /proc, /sys, /run),
    size is shown as '-' to avoid expensive recursive traversal.
  - --limit N returns at most N listed results. With --sort, unearth selects
    only the best N entries before sorting that subset. --counts ignores it.
  - --reverse reverses the final result order. When combined with --limit,
    the limit is selected first, then the selected results are reversed.
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
        limit: None,
        reverse: false,
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
        index_if_watched: false,
        index_binary: false,
        recent_limit: None,
        index_refresh: None,
        index_snapshot: None,
        index_purge: None,
        watch: false,
        watch_status: false,
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
            "--limit" => {
                i += 1;
                if i < args.len() {
                    let parsed = args[i]
                        .parse::<usize>()
                        .map_err(|_| "--limit requires a positive integer".to_string())?;
                    if parsed == 0 {
                        return Err("--limit requires a positive integer".to_string());
                    }
                    opts.limit = Some(parsed);
                } else {
                    return Err("--limit requires a count".to_string());
                }
            }
            _ if arg.starts_with("--limit=") => {
                let parsed = arg
                    .trim_start_matches("--limit=")
                    .parse::<usize>()
                    .map_err(|_| "--limit requires a positive integer".to_string())?;
                if parsed == 0 {
                    return Err("--limit requires a positive integer".to_string());
                }
                opts.limit = Some(parsed);
            }
            "--reverse" => opts.reverse = true,
            "--no-recurse" | "-R" => opts.no_recurse = true,
            "--follow-links" => opts.follow_links = true,
            "--ignore" => opts.respect_ignore = true,
            "--hidden" | "-H" => opts.visible_only = false,
            "--cache-raw" => opts.cache_output = true,
            "--snapshot-cache" => opts.snapshot_cache = true,
            "--snapshot-refresh" => opts.snapshot_refresh = true,
            "--index" => opts.index_mode = true,
            "--index-if-watched" => opts.index_if_watched = true,
            "--index-binary" => {
                opts.index_mode = true;
                opts.index_binary = true;
            }
            "--recent" => {
                i += 1;
                if i < args.len() {
                    opts.recent_limit = Some(
                        args[i]
                            .parse::<usize>()
                            .map_err(|_| "--recent requires a positive integer".to_string())?,
                    );
                    if opts.recent_limit == Some(0) {
                        return Err("--recent requires a positive integer".to_string());
                    }
                    opts.index_mode = true;
                } else {
                    return Err("--recent requires a count".to_string());
                }
            }
            _ if arg.starts_with("--recent=") => {
                let value = arg.trim_start_matches("--recent=");
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| "--recent requires a positive integer".to_string())?;
                if parsed == 0 {
                    return Err("--recent requires a positive integer".to_string());
                }
                opts.recent_limit = Some(parsed);
                opts.index_mode = true;
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
            "--index-snapshot" => {
                i += 1;
                if i < args.len() {
                    opts.index_snapshot = Some(args[i].clone());
                } else {
                    return Err("--index-snapshot requires a root path".to_string());
                }
            }
            _ if arg.starts_with("--index-snapshot=") => {
                let v = arg.trim_start_matches("--index-snapshot=").to_string();
                if v.is_empty() {
                    return Err("--index-snapshot requires a non-empty root path".to_string());
                }
                opts.index_snapshot = Some(v);
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
            "--watch" => opts.watch = true,
            "--watch-status" => opts.watch_status = true,
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

    if opts.positional.is_empty()
        && opts.index_refresh.is_none()
        && opts.index_snapshot.is_none()
        && opts.index_purge.is_none()
        && opts.recent_limit.is_none()
        && !opts.watch_status
        && !opts.watch
    {
        return Err(usage());
    }
    let maintenance_modes = [
        opts.index_refresh.is_some(),
        opts.index_snapshot.is_some(),
        opts.index_purge.is_some(),
        opts.watch,
        opts.watch_status,
    ]
    .into_iter()
    .filter(|enabled| *enabled)
    .count();
    if maintenance_modes > 1 {
        return Err(
            "--watch, --watch-status, --index-refresh, --index-snapshot, and --index-purge are mutually exclusive"
                .to_string(),
        );
    }
    if opts.watch
        && (opts.index_mode
            || opts.index_if_watched
            || opts.recent_limit.is_some()
            || opts.snapshot_cache
            || opts.snapshot_refresh)
    {
        return Err("--watch cannot be combined with search or snapshot modes".to_string());
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
        && !opts.reverse
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

#[allow(clippy::too_many_arguments)]
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
        if is_dir
            && !no_recurse
            && !(dir.as_os_str().as_bytes() == b"/"
                && (name_bytes == b"proc"
                    || name_bytes == b"sys"
                    || name_bytes == b"dev"
                    || name_bytes == b"run"))
        {
            if is_symlink && !follow_links {
                continue;
            }
            subdirs.push(path);
        }
    }
    if !local_buf.is_empty() && tx.send(local_buf).is_err() {
        return;
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

#[allow(clippy::too_many_arguments)]
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
            } else if full_path_match {
                if re.is_match(name_lossy.as_ref()) {
                    true
                } else {
                    let match_target = path.to_string_lossy();
                    re.is_match(&match_target)
                }
            } else {
                re.is_match(name_lossy.as_ref())
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
                    indexed_activity_nanos: None,
                    indexed_size: None,
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
    if !local_buf.is_empty() && tx.send(local_buf).is_err() {
        return;
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
    if let Some(Ok(data_item)) = file.data(device, "") {
        if let Ok(data_attr_obj) = data_item.to_attribute() {
            if let Ok(value) = data_attr_obj.value(device) {
                return value.len();
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
    item.metadata
        .as_ref()
        .map(|m| m.len())
        .or(item.indexed_size)
        .unwrap_or(0)
}

fn result_activity_nanos(item: &SearchResult) -> Option<i64> {
    item.indexed_activity_nanos.or_else(|| {
        item.metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(system_time_to_unix_nanos)
    })
}

fn result_activity_time(item: &SearchResult) -> Option<std::time::SystemTime> {
    let nanos = u64::try_from(result_activity_nanos(item)?).ok()?;
    std::time::UNIX_EPOCH.checked_add(Duration::from_nanos(nanos))
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
        if let Some(limit) = opts.limit {
            items.truncate(limit);
        }
        return items;
    };
    let order = opts.sort_order.unwrap_or(SortOrder::Asc);
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
            SortField::Name => a.path.to_lowercase().cmp(&b.path.to_lowercase()),
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

fn index_state_path(root_key: &str, suffix: &str) -> Option<PathBuf> {
    Some(unearth_cache_dir()?.join("index").join(format!(
        "{:016x}.{}",
        stable_root_hash(root_key),
        suffix
    )))
}

fn stable_root_hash(root_key: &str) -> u64 {
    root_key.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn index_snapshot_path(root_key: &str) -> Option<PathBuf> {
    Some(
        unearth_cache_dir()?
            .join("index")
            .join(format!("{:016x}.snapshot", stable_root_hash(root_key))),
    )
}

fn index_manifest_path(root_key: &str) -> Option<PathBuf> {
    Some(
        unearth_cache_dir()?
            .join("index")
            .join(format!("{:016x}.manifest", stable_root_hash(root_key))),
    )
}

fn unique_temp_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}.{}", std::process::id(), nanos)
}

fn sync_parent_dir(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "cache file has no parent directory".to_string())?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())
}

fn index_delta_path(root_key: &str) -> Option<PathBuf> {
    Some(
        unearth_cache_dir()?
            .join("index")
            .join(format!("{:016x}.delta", stable_root_hash(root_key))),
    )
}

fn write_len_prefixed(writer: &mut impl Write, value: &str) -> Result<(), String> {
    let len = u32::try_from(value.len()).map_err(|_| "index string is too long".to_string())?;
    writer
        .write_all(&len.to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(value.as_bytes())
        .map_err(|e| e.to_string())
}

fn read_u32_bytes(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    let end = cursor.saturating_add(4);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated index sidecar".to_string())?;
    *cursor = end;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn read_u64_bytes(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let end = cursor.saturating_add(8);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated index sidecar".to_string())?;
    *cursor = end;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

fn read_i64_bytes(bytes: &[u8], cursor: &mut usize) -> Result<i64, String> {
    Ok(read_u64_bytes(bytes, cursor)? as i64)
}

fn decode_optional_i64(value: i64) -> Option<i64> {
    (value != i64::MIN).then_some(value)
}

fn encode_optional_i64(value: Option<i64>) -> i64 {
    value.unwrap_or(i64::MIN)
}

fn read_string_bytes<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a str, String> {
    let len = read_u32_bytes(bytes, cursor)? as usize;
    let end = cursor.saturating_add(len);
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| "truncated index sidecar string".to_string())?;
    *cursor = end;
    std::str::from_utf8(value).map_err(|e| e.to_string())
}

fn open_index_manifest(root_key: &str) -> Result<Option<Mmap>, String> {
    let Some(path) = index_manifest_path(root_key) else {
        return Ok(None);
    };
    let Ok(file) = File::open(path) else {
        return Ok(None);
    };
    // The manifest is replaced atomically and remains immutable while mapped.
    let mmap = unsafe { Mmap::map(&file) }.map_err(|e| e.to_string())?;
    Ok(Some(mmap))
}

fn parse_index_manifest<'a>(
    mmap: &'a Mmap,
    root_key: &str,
    fingerprint: &str,
) -> Result<Vec<ExistingIndexEntry<'a>>, String> {
    let bytes = mmap.as_ref();
    if bytes.get(..8) != Some(INDEX_MANIFEST_MAGIC) {
        return Err("unsupported index manifest version".to_string());
    }
    let mut cursor = 8;
    if read_string_bytes(bytes, &mut cursor)? != root_key {
        return Err("index manifest root mismatch".to_string());
    }
    if read_string_bytes(bytes, &mut cursor)? != fingerprint {
        return Err("index manifest fingerprint mismatch".to_string());
    }
    let count = usize::try_from(read_u64_bytes(bytes, &mut cursor)?)
        .map_err(|_| "index manifest is too large".to_string())?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let dir_id = read_i64_bytes(bytes, &mut cursor)?;
        let name_id = read_i64_bytes(bytes, &mut cursor)?;
        let kind = *bytes
            .get(cursor)
            .ok_or_else(|| "truncated index manifest kind".to_string())? as i64;
        cursor += 1;
        let mtime = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let size = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let activity = decode_optional_i64(read_i64_bytes(bytes, &mut cursor)?);
        let path = read_string_bytes(bytes, &mut cursor)?;
        entries.push(ExistingIndexEntry {
            dir_id,
            name_id,
            path: Cow::Borrowed(path),
            kind,
            mtime,
            size,
            activity,
        });
    }
    if cursor != bytes.len() {
        return Err("index manifest has trailing data".to_string());
    }
    Ok(entries)
}

fn begin_index_manifest(
    root_key: &str,
    fingerprint: &str,
    count: usize,
) -> Result<(PathBuf, PathBuf, BufWriter<File>), String> {
    let path = index_manifest_path(root_key)
        .ok_or_else(|| "unable to resolve index manifest path".to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "invalid manifest path".to_string())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("manifest.tmp.{}", unique_temp_tag()));
    let mut writer =
        BufWriter::with_capacity(1024 * 1024, File::create(&tmp).map_err(|e| e.to_string())?);
    writer
        .write_all(INDEX_MANIFEST_MAGIC)
        .map_err(|e| e.to_string())?;
    write_len_prefixed(&mut writer, root_key)?;
    write_len_prefixed(&mut writer, fingerprint)?;
    writer
        .write_all(&(count as u64).to_le_bytes())
        .map_err(|e| e.to_string())?;
    Ok((path, tmp, writer))
}

fn write_index_manifest_record(
    writer: &mut impl Write,
    entry: &PendingIndexEntry,
    path: &str,
) -> Result<(), String> {
    writer
        .write_all(&entry.dir_id.to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&entry.name_id.to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&[entry.kind as u8])
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.mtime).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.size).to_le_bytes())
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&encode_optional_i64(entry.activity).to_le_bytes())
        .map_err(|e| e.to_string())?;
    write_len_prefixed(writer, path)
}

fn finish_index_manifest(
    path: PathBuf,
    tmp: PathBuf,
    mut writer: BufWriter<File>,
) -> Result<(), String> {
    writer.flush().map_err(|e| e.to_string())?;
    writer.get_ref().sync_all().map_err(|e| e.to_string())?;
    drop(writer);
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    sync_parent_dir(&path)
}

fn write_index_snapshot_from_db(conn: &Connection, root_key: &str) -> Result<(), String> {
    let snapshot_path = index_snapshot_path(root_key)
        .ok_or_else(|| "unable to resolve the unearth cache directory".to_string())?;
    let parent = snapshot_path
        .parent()
        .ok_or_else(|| "invalid index snapshot path".to_string())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp_path = snapshot_path.with_extension(format!("snapshot.tmp.{}", unique_temp_tag()));

    let result = (|| -> Result<(), String> {
        let file = File::create(&tmp_path).map_err(|e| e.to_string())?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);
        writer
            .write_all(INDEX_SNAPSHOT_MAGIC)
            .map_err(|e| e.to_string())?;
        let root_len = u32::try_from(root_key.len())
            .map_err(|_| "indexed root path is too long".to_string())?;
        writer
            .write_all(&root_len.to_le_bytes())
            .map_err(|e| e.to_string())?;
        writer
            .write_all(root_key.as_bytes())
            .map_err(|e| e.to_string())?;
        let counts_offset = writer.stream_position().map_err(|e| e.to_string())?;
        writer.write_all(&[0u8; 24]).map_err(|e| e.to_string())?;

        let root_prefix = index_path_prefix(root_key);
        let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
        let mut stmt = conn
            .prepare(
                "SELECT d.path, s.value, e.kind
                 FROM dirs d INDEXED BY idx_dirs_path
                 CROSS JOIN entries e INDEXED BY idx_entries_dir ON e.dir_id = d.id
                 JOIN strings s ON e.name_id = s.id
                 WHERE d.path = ?1 OR (d.path >= ?2 AND d.path < ?3)",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(params![root_key, root_prefix, root_prefix_end])
            .map_err(|e| e.to_string())?;
        let mut all_count = 0u64;
        let mut file_count = 0u64;
        let mut dir_count = 0u64;

        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let dir_path: String = row.get(0).map_err(|e| e.to_string())?;
            let name: String = row.get(1).map_err(|e| e.to_string())?;
            let kind: u8 = row.get(2).map_err(|e| e.to_string())?;
            let mut path = if dir_path == "/" {
                format!("/{}", name)
            } else {
                format!("{}/{}", dir_path, name)
            };
            if kind == 1 {
                path.push('/');
                dir_count = dir_count.saturating_add(1);
            } else if kind == 0 {
                file_count = file_count.saturating_add(1);
            }
            all_count = all_count.saturating_add(1);

            let path_len = u32::try_from(path.len())
                .map_err(|_| format!("indexed path is too long: {}", path))?;
            writer.write_all(&[kind]).map_err(|e| e.to_string())?;
            writer
                .write_all(&path_len.to_le_bytes())
                .map_err(|e| e.to_string())?;
            writer
                .write_all(path.as_bytes())
                .map_err(|e| e.to_string())?;
        }

        writer.flush().map_err(|e| e.to_string())?;
        writer
            .seek(SeekFrom::Start(counts_offset))
            .map_err(|e| e.to_string())?;
        for count in [all_count, file_count, dir_count] {
            writer
                .write_all(&count.to_le_bytes())
                .map_err(|e| e.to_string())?;
        }
        writer.flush().map_err(|e| e.to_string())?;
        writer.get_ref().sync_all().map_err(|e| e.to_string())?;
        drop(writer);
        fs::rename(&tmp_path, &snapshot_path).map_err(|e| e.to_string())?;
        fs::set_permissions(&snapshot_path, fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        sync_parent_dir(&snapshot_path)?;
        if let Some(legacy_path) = index_state_path(root_key, "snapshot") {
            if legacy_path != snapshot_path {
                let _ = fs::remove_file(legacy_path);
            }
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn rebuild_index_snapshot(root_raw: &str) -> Result<(), String> {
    let root_key = normalize_index_root_arg(root_raw)?;
    let conn = open_index_db()?;
    if !index_root_is_known(&conn, &root_key)? {
        return Err(format!(
            "'{}' is not present in the unearth index",
            root_key
        ));
    }
    let fingerprint_key = index_fingerprint_key(&root_key);
    let fingerprint = index_fingerprint_value(&conn, &fingerprint_key)?
        .ok_or_else(|| format!("'{}' has no committed fingerprint", root_key))?;
    rebuild_index_base_sidecars(&conn, &root_key, &fingerprint)
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
    if File::create(path).is_ok() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

fn open_index_db() -> Result<Connection, String> {
    let path =
        index_db_path().ok_or_else(|| "Could not determine unearth cache dir".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        if let Some(cache_root) = parent.parent() {
            fs::set_permissions(cache_root, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
    }
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(30))
        .map_err(|e| e.to_string())?;
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
            activity INTEGER,
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
        CREATE TABLE IF NOT EXISTS actors (
            id INTEGER PRIMARY KEY,
            executable TEXT NOT NULL UNIQUE,
            classification TEXT NOT NULL,
            first_seen INTEGER NOT NULL,
            last_seen INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS watch_state (
            root TEXT PRIMARY KEY,
            backend TEXT NOT NULL,
            status TEXT NOT NULL,
            generation INTEGER NOT NULL DEFAULT 0,
            last_event INTEGER,
            last_reconcile INTEGER,
            dirty INTEGER NOT NULL DEFAULT 0,
            online INTEGER NOT NULL DEFAULT 1,
            watcher_pid INTEGER,
            error TEXT,
            owner_boot_id TEXT,
            owner_starttime INTEGER,
            heartbeat INTEGER
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
    let mut stmt = conn
        .prepare("PRAGMA table_info(entries)")
        .map_err(|e| e.to_string())?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    if !columns.iter().any(|column| column == "activity") {
        conn.execute("ALTER TABLE entries ADD COLUMN activity INTEGER", [])
            .map_err(|e| e.to_string())?;
    }
    for column in [
        "event_kind INTEGER",
        "actor_id INTEGER",
        "actor_uid INTEGER",
        "actor_pid INTEGER",
        "event_at INTEGER",
    ] {
        let name = column.split_whitespace().next().unwrap_or_default();
        if !columns.iter().any(|existing| existing == name) {
            conn.execute(&format!("ALTER TABLE entries ADD COLUMN {column}"), [])
                .map_err(|e| e.to_string())?;
        }
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_entries_kind_activity ON entries(kind, activity DESC)",
        [],
    )
    .map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("PRAGMA table_info(watch_state)")
        .map_err(|e| e.to_string())?;
    let watch_columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    for column in [
        "owner_boot_id TEXT",
        "owner_starttime INTEGER",
        "heartbeat INTEGER",
    ] {
        let name = column.split_whitespace().next().unwrap_or_default();
        if !watch_columns.iter().any(|existing| existing == name) {
            conn.execute(&format!("ALTER TABLE watch_state ADD COLUMN {column}"), [])
                .map_err(|e| e.to_string())?;
        }
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if sidecar.is_file() {
            fs::set_permissions(sidecar, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
    }
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

#[derive(Clone, Copy)]
struct PendingIndexEntry {
    dir_id: i64,
    name_id: i64,
    kind: i64,
    mtime: Option<i64>,
    size: Option<i64>,
    activity: Option<i64>,
}

#[derive(Debug, Eq, PartialEq)]
struct ScannedIndexEntry {
    path: String,
    kind: i64,
    mtime: Option<i64>,
    size: Option<i64>,
    activity: Option<i64>,
}

#[derive(Debug, Eq, PartialEq)]
struct ExistingIndexEntry<'a> {
    dir_id: i64,
    name_id: i64,
    path: Cow<'a, str>,
    kind: i64,
    mtime: Option<i64>,
    size: Option<i64>,
    activity: Option<i64>,
}

#[derive(Clone, Debug)]
struct IndexDeltaEntry {
    added: bool,
    dir_id: i64,
    name_id: i64,
    kind: i64,
    path: String,
}

fn scan_index_root(
    root: &Path,
    root_key: &str,
    threads: usize,
) -> Result<Vec<ScannedIndexEntry>, String> {
    let mut entries: Vec<ScannedIndexEntry> = WalkDir::new(root)
        .skip_hidden(false)
        .parallelism(Parallelism::RayonNewPool(threads))
        .process_read_dir({
            let root_key = root_key.to_string();
            move |_depth, _path, _state, children| {
                for entry in children.iter_mut().flatten() {
                    if let Some(child_path) = entry.read_children_path.as_ref() {
                        if is_root_index_prune_child(&root_key, child_path.as_ref()) {
                            entry.read_children_path = None;
                        }
                    }
                }
            }
        })
        .into_iter()
        .filter_map(|entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error.to_string())),
            };
            let path = entry.path();
            if path == root || path.file_name().is_none() || path.parent().is_none() {
                return None;
            }
            let path = normalize_index_dir(&path);
            if is_root_index_excluded_path(root_key, &path) {
                return None;
            }
            let file_type = entry.file_type();
            let kind = if file_type.is_dir() {
                1
            } else if file_type.is_symlink() {
                2
            } else {
                0
            };
            Some(Ok(ScannedIndexEntry {
                path,
                kind,
                mtime: None,
                size: None,
                activity: None,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let populate = |entry: &mut ScannedIndexEntry| {
        let metadata = fs::symlink_metadata(&entry.path).ok();
        entry.mtime = metadata.as_ref().and_then(metadata_mtime_nanos);
        entry.size = metadata.as_ref().and_then(metadata_size_i64);
        entry.activity = metadata.as_ref().and_then(metadata_activity_nanos);
    };
    if threads == 1 {
        entries.iter_mut().for_each(populate);
    } else if let Ok(pool) = ThreadPoolBuilder::new().num_threads(threads).build() {
        pool.install(|| entries.par_iter_mut().for_each(populate));
    } else {
        entries.iter_mut().for_each(populate);
    }
    Ok(entries)
}

fn mix_index_fingerprint(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58476d1ce4e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn index_fingerprint(entries: &[ScannedIndexEntry]) -> String {
    let mut xor = 0u64;
    let mut sum = 0u64;
    let mut rotated_sum = 0u64;
    for entry in entries {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in entry.path.as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
        hash = (hash ^ entry.kind as u64).wrapping_mul(0x100000001b3);
        let mixed = mix_index_fingerprint(hash);
        xor ^= mixed;
        sum = sum.wrapping_add(mixed);
        rotated_sum = rotated_sum.wrapping_add(mixed.rotate_left((mixed & 63) as u32));
    }
    format!("{}:{xor:016x}:{sum:016x}:{rotated_sum:016x}", entries.len())
}

fn index_fingerprint_key(root_key: &str) -> String {
    format!("root_fingerprint_v1:{:016x}", stable_root_hash(root_key))
}

fn index_fingerprint_value(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    match conn.query_row(
        "SELECT value FROM index_meta WHERE key = ?1",
        [key],
        |row| row.get(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn load_index_id_map(
    conn: &Connection,
    sql: &str,
    query_params: impl rusqlite::Params,
) -> Result<HashMap<String, i64>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(query_params, |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(0)?))
        })
        .map_err(|e| e.to_string())?;
    let mut ids = HashMap::new();
    for row in rows {
        let (value, id) = row.map_err(|e| e.to_string())?;
        ids.insert(value, id);
    }
    Ok(ids)
}

fn next_index_id(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(
        &format!("SELECT COALESCE(MAX(id), 0) + 1 FROM {table}"),
        [],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

fn system_time_to_unix_nanos(value: std::time::SystemTime) -> Option<i64> {
    let duration = value.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_nanos()).ok()
}

fn metadata_mtime_nanos(metadata: &fs::Metadata) -> Option<i64> {
    metadata.modified().ok().and_then(system_time_to_unix_nanos)
}

fn metadata_activity_nanos(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata_mtime_nanos(metadata);
    let created = metadata.created().ok().and_then(system_time_to_unix_nanos);
    match (modified, created) {
        (Some(m), Some(c)) => Some(m.max(c)),
        (Some(m), None) => Some(m),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

fn metadata_size_i64(metadata: &fs::Metadata) -> Option<i64> {
    i64::try_from(metadata.len()).ok()
}

fn ensure_index_id(
    stmt: &mut rusqlite::Statement<'_>,
    ids: &mut HashMap<String, i64>,
    next_id: &mut i64,
    value: &str,
) -> Result<i64, String> {
    if let Some(id) = ids.get(value) {
        return Ok(*id);
    }
    let id = *next_id;
    *next_id = next_id
        .checked_add(1)
        .ok_or_else(|| "index ID space exhausted".to_string())?;
    stmt.execute(params![id, value])
        .map_err(|e| e.to_string())?;
    ids.insert(value.to_string(), id);
    Ok(id)
}

fn insert_index_entries(
    tx: &rusqlite::Transaction<'_>,
    entries: &[PendingIndexEntry],
) -> Result<(), String> {
    for batch in entries.chunks(INDEX_INSERT_BATCH_SIZE) {
        let values = std::iter::repeat_n("(?, ?, ?, ?, ?, ?)", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO entries(dir_id, name_id, kind, mtime, size, activity) VALUES {values}
             ON CONFLICT(dir_id, name_id, kind) DO UPDATE SET
                 mtime=excluded.mtime,
                 size=excluded.size,
                 activity=excluded.activity"
        );
        let mut params = Vec::<rusqlite::types::Value>::with_capacity(batch.len() * 6);
        for entry in batch {
            params.push(entry.dir_id.into());
            params.push(entry.name_id.into());
            params.push(entry.kind.into());
            params.push(entry.mtime.into());
            params.push(entry.size.into());
            params.push(entry.activity.into());
        }
        tx.prepare_cached(&sql)
            .map_err(|e| e.to_string())?
            .execute(params_from_iter(params))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn load_existing_index_entries(
    conn: &Connection,
    root_key: &str,
    root_prefix: &str,
    root_prefix_end: &str,
) -> Result<Vec<ExistingIndexEntry<'static>>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT e.dir_id, e.name_id, d.path, s.value, e.kind, e.mtime, e.size, e.activity
             FROM dirs d INDEXED BY idx_dirs_path
             CROSS JOIN entries e INDEXED BY idx_entries_dir ON e.dir_id = d.id
             JOIN strings s ON e.name_id = s.id
             WHERE d.path = ?1 OR (d.path >= ?2 AND d.path < ?3)",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![root_key, root_prefix, root_prefix_end], |row| {
            let dir_id = row.get(0)?;
            let name_id = row.get(1)?;
            let dir: String = row.get(2)?;
            let name: String = row.get(3)?;
            let kind = row.get(4)?;
            let mtime = row.get(5)?;
            let size = row.get(6)?;
            let activity = row.get(7)?;
            let path = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            Ok(ExistingIndexEntry {
                dir_id,
                name_id,
                path: Cow::Owned(path),
                kind,
                mtime,
                size,
                activity,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn compare_index_entry(path_a: &str, kind_a: i64, path_b: &str, kind_b: i64) -> CmpOrdering {
    path_a.cmp(path_b).then_with(|| kind_a.cmp(&kind_b))
}

fn sort_and_dedup_scanned_index_entries(entries: &mut Vec<ScannedIndexEntry>) {
    entries.par_sort_unstable_by(|a, b| compare_index_entry(&a.path, a.kind, &b.path, b.kind));
    entries.dedup_by(|current, previous| {
        current.path == previous.path && current.kind == previous.kind
    });
}

fn diff_index_entries(
    scanned: &[ScannedIndexEntry],
    existing: &[ExistingIndexEntry<'_>],
) -> (Vec<usize>, Vec<usize>, Vec<(usize, usize)>) {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let (mut scan_idx, mut existing_idx) = (0, 0);

    while scan_idx < scanned.len() && existing_idx < existing.len() {
        match compare_index_entry(
            &scanned[scan_idx].path,
            scanned[scan_idx].kind,
            &existing[existing_idx].path,
            existing[existing_idx].kind,
        ) {
            CmpOrdering::Less => {
                added.push(scan_idx);
                scan_idx += 1;
            }
            CmpOrdering::Greater => {
                removed.push(existing_idx);
                existing_idx += 1;
            }
            CmpOrdering::Equal => {
                if scanned[scan_idx].mtime != existing[existing_idx].mtime
                    || scanned[scan_idx].size != existing[existing_idx].size
                    || scanned[scan_idx].activity != existing[existing_idx].activity
                {
                    updated.push((existing_idx, scan_idx));
                }
                scan_idx += 1;
                existing_idx += 1;
            }
        }
    }
    added.extend(scan_idx..scanned.len());
    removed.extend(existing_idx..existing.len());
    (removed, added, updated)
}

fn delete_index_entries(
    tx: &rusqlite::Transaction<'_>,
    existing: &[ExistingIndexEntry<'_>],
    entry_indices: &[usize],
) -> Result<(), String> {
    for batch in entry_indices.chunks(INDEX_INSERT_BATCH_SIZE) {
        let placeholders = std::iter::repeat_n("(?, ?, ?)", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM entries WHERE (dir_id, name_id, kind) IN ({placeholders})");
        let values = batch.iter().flat_map(|index| {
            let entry = &existing[*index];
            [entry.dir_id, entry.name_id, entry.kind]
        });
        tx.prepare_cached(&sql)
            .map_err(|e| e.to_string())?
            .execute(params_from_iter(values))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn update_index_entries_metadata(
    tx: &rusqlite::Transaction<'_>,
    existing: &[ExistingIndexEntry<'_>],
    scanned: &[ScannedIndexEntry],
    updates: &[(usize, usize)],
) -> Result<(), String> {
    let mut stmt = tx
        .prepare_cached(
            "UPDATE entries
             SET mtime = ?1, size = ?2, activity = ?3
             WHERE dir_id = ?4 AND name_id = ?5 AND kind = ?6",
        )
        .map_err(|e| e.to_string())?;
    for &(existing_idx, scanned_idx) in updates {
        let old = &existing[existing_idx];
        let new = &scanned[scanned_idx];
        stmt.execute(params![
            new.mtime,
            new.size,
            new.activity,
            old.dir_id,
            old.name_id,
            old.kind,
        ])
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn apply_index_metadata_updates(
    existing: &mut [ExistingIndexEntry<'_>],
    scanned: &[ScannedIndexEntry],
    updates: &[(usize, usize)],
) {
    for &(existing_idx, scanned_idx) in updates {
        let old = &mut existing[existing_idx];
        let new = &scanned[scanned_idx];
        old.mtime = new.mtime;
        old.size = new.size;
        old.activity = new.activity;
    }
}

fn get_or_insert_index_id(
    select: &mut rusqlite::Statement<'_>,
    insert: &mut rusqlite::Statement<'_>,
    cache: &mut HashMap<String, i64>,
    value: &str,
) -> Result<i64, String> {
    if let Some(id) = cache.get(value) {
        return Ok(*id);
    }
    let id = match select.query_row([value], |row| row.get(0)) {
        Ok(id) => id,
        Err(rusqlite::Error::QueryReturnedNoRows) => insert
            .query_row([value], |row| row.get(0))
            .map_err(|e| e.to_string())?,
        Err(error) => return Err(error.to_string()),
    };
    cache.insert(value.to_string(), id);
    Ok(id)
}

fn incremental_change_limit(existing_len: usize, scanned_len: usize) -> usize {
    let proportional = existing_len
        .max(scanned_len)
        .div_ceil(INDEX_INCREMENTAL_CHANGE_DIVISOR);
    proportional.clamp(1_024, INDEX_INCREMENTAL_MAX_CHANGES)
}

fn write_manifest_from_existing(
    root_key: &str,
    fingerprint: &str,
    entries: &[ExistingIndexEntry<'_>],
) -> Result<(), String> {
    let (path, tmp, mut writer) = begin_index_manifest(root_key, fingerprint, entries.len())?;
    for entry in entries {
        write_index_manifest_record(
            &mut writer,
            &PendingIndexEntry {
                dir_id: entry.dir_id,
                name_id: entry.name_id,
                kind: entry.kind,
                mtime: entry.mtime,
                size: entry.size,
                activity: entry.activity,
            },
            &entry.path,
        )?;
    }
    finish_index_manifest(path, tmp, writer)
}

fn write_manifest_from_scanned(
    root_key: &str,
    fingerprint: &str,
    scanned: &[ScannedIndexEntry],
    entries: &[PendingIndexEntry],
) -> Result<(), String> {
    if scanned.len() != entries.len() {
        return Err("manifest entry count mismatch".to_string());
    }
    let (path, tmp, mut writer) = begin_index_manifest(root_key, fingerprint, scanned.len())?;
    for (scanned, entry) in scanned.iter().zip(entries) {
        write_index_manifest_record(&mut writer, entry, &scanned.path)?;
    }
    finish_index_manifest(path, tmp, writer)
}

fn write_incremental_manifest(
    root_key: &str,
    fingerprint: &str,
    scanned: &[ScannedIndexEntry],
    existing: &[ExistingIndexEntry<'_>],
    added: &HashMap<usize, PendingIndexEntry>,
) -> Result<(), String> {
    let (path, tmp, mut writer) = begin_index_manifest(root_key, fingerprint, scanned.len())?;
    let mut existing_idx = 0;
    for (scan_idx, scanned_entry) in scanned.iter().enumerate() {
        if let Some(entry) = added.get(&scan_idx) {
            write_index_manifest_record(&mut writer, entry, &scanned_entry.path)?;
            continue;
        }
        while existing_idx < existing.len()
            && compare_index_entry(
                &existing[existing_idx].path,
                existing[existing_idx].kind,
                &scanned_entry.path,
                scanned_entry.kind,
            ) == CmpOrdering::Less
        {
            existing_idx += 1;
        }
        let Some(entry) = existing.get(existing_idx) else {
            return Err("incremental manifest lost an unchanged entry".to_string());
        };
        if compare_index_entry(
            &entry.path,
            entry.kind,
            &scanned_entry.path,
            scanned_entry.kind,
        ) != CmpOrdering::Equal
        {
            return Err("incremental manifest entry mismatch".to_string());
        }
        write_index_manifest_record(
            &mut writer,
            &PendingIndexEntry {
                dir_id: entry.dir_id,
                name_id: entry.name_id,
                kind: entry.kind,
                mtime: entry.mtime,
                size: entry.size,
                activity: entry.activity,
            },
            &scanned_entry.path,
        )?;
        existing_idx += 1;
    }
    finish_index_manifest(path, tmp, writer)
}

fn read_index_delta(
    root_key: &str,
    fingerprint: &str,
) -> Result<BTreeMap<(String, i64), IndexDeltaEntry>, String> {
    let Some(path) = index_delta_path(root_key) else {
        return Ok(BTreeMap::new());
    };
    let Ok(bytes) = fs::read(path) else {
        return Ok(BTreeMap::new());
    };
    if bytes.get(..8) != Some(INDEX_DELTA_MAGIC) {
        return Err("unsupported index delta version".to_string());
    }
    let mut cursor = 8;
    if read_string_bytes(&bytes, &mut cursor)? != root_key {
        return Err("index delta root mismatch".to_string());
    }
    if read_string_bytes(&bytes, &mut cursor)? != fingerprint {
        return Err("index delta fingerprint mismatch".to_string());
    }
    let _all_count = read_u64_bytes(&bytes, &mut cursor)?;
    let _file_count = read_u64_bytes(&bytes, &mut cursor)?;
    let _dir_count = read_u64_bytes(&bytes, &mut cursor)?;
    let count = usize::try_from(read_u64_bytes(&bytes, &mut cursor)?)
        .map_err(|_| "index delta is too large".to_string())?;
    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let added = *bytes
            .get(cursor)
            .ok_or_else(|| "truncated index delta action".to_string())?
            != 0;
        cursor += 1;
        let dir_id = read_i64_bytes(&bytes, &mut cursor)?;
        let name_id = read_i64_bytes(&bytes, &mut cursor)?;
        let kind = *bytes
            .get(cursor)
            .ok_or_else(|| "truncated index delta kind".to_string())? as i64;
        cursor += 1;
        let path = read_string_bytes(&bytes, &mut cursor)?.to_string();
        entries.insert(
            (path.clone(), kind),
            IndexDeltaEntry {
                added,
                dir_id,
                name_id,
                kind,
                path,
            },
        );
    }
    Ok(entries)
}

fn write_index_delta(
    root_key: &str,
    fingerprint: &str,
    scanned: &[ScannedIndexEntry],
    entries: &BTreeMap<(String, i64), IndexDeltaEntry>,
) -> Result<(), String> {
    if entries.is_empty() {
        remove_index_delta(root_key);
        return Ok(());
    }
    let path = index_delta_path(root_key)
        .ok_or_else(|| "unable to resolve index delta path".to_string())?;
    let tmp = path.with_extension(format!("delta.tmp.{}", unique_temp_tag()));
    let mut writer =
        BufWriter::with_capacity(1024 * 1024, File::create(&tmp).map_err(|e| e.to_string())?);
    writer
        .write_all(INDEX_DELTA_MAGIC)
        .map_err(|e| e.to_string())?;
    write_len_prefixed(&mut writer, root_key)?;
    write_len_prefixed(&mut writer, fingerprint)?;
    let all_count = scanned.len() as u64;
    let file_count = scanned.iter().filter(|entry| entry.kind == 0).count() as u64;
    let dir_count = scanned.iter().filter(|entry| entry.kind == 1).count() as u64;
    for value in [all_count, file_count, dir_count, entries.len() as u64] {
        writer
            .write_all(&value.to_le_bytes())
            .map_err(|e| e.to_string())?;
    }
    for entry in entries.values() {
        writer
            .write_all(&[u8::from(entry.added)])
            .map_err(|e| e.to_string())?;
        writer
            .write_all(&entry.dir_id.to_le_bytes())
            .map_err(|e| e.to_string())?;
        writer
            .write_all(&entry.name_id.to_le_bytes())
            .map_err(|e| e.to_string())?;
        writer
            .write_all(&[entry.kind as u8])
            .map_err(|e| e.to_string())?;
        write_len_prefixed(&mut writer, &entry.path)?;
    }
    writer.flush().map_err(|e| e.to_string())?;
    writer.get_ref().sync_all().map_err(|e| e.to_string())?;
    drop(writer);
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    sync_parent_dir(&path)
}

fn remove_index_delta(root_key: &str) {
    if let Some(path) = index_delta_path(root_key) {
        let _ = fs::remove_file(path);
    }
}

fn rebuild_index_base_sidecars(
    conn: &Connection,
    root_key: &str,
    fingerprint: &str,
) -> Result<(), String> {
    // Readers may briefly see the old base without a delta, but never a new base
    // combined with tombstones that were defined against the old base.
    remove_index_delta(root_key);
    write_index_snapshot_from_db(conn, root_key)?;
    let root_prefix = index_path_prefix(root_key);
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let mut entries = load_existing_index_entries(conn, root_key, &root_prefix, &root_prefix_end)?;
    entries.par_sort_unstable_by(|a, b| compare_index_entry(&a.path, a.kind, &b.path, b.kind));
    write_manifest_from_existing(root_key, fingerprint, &entries)?;
    Ok(())
}

fn index_sidecars_are_current(root_key: &str, fingerprint: &str) -> bool {
    let snapshot_exists = index_snapshot_path(root_key).is_some_and(|path| path.is_file());
    if !snapshot_exists {
        return false;
    }
    let Ok(Some(mmap)) = open_index_manifest(root_key) else {
        return false;
    };
    if parse_index_manifest(&mmap, root_key, fingerprint).is_err() {
        return false;
    }
    read_index_delta(root_key, fingerprint).is_ok()
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
    if !raw.is_empty() && raw.bytes().all(|byte| byte == b'*') {
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
    if let Some(stripped) = raw.strip_prefix('/') {
        params.push(format!("{}%", sql_like_escape(&stripped.to_lowercase())));
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
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
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
            SELECT id FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)
        )",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM indexed_roots WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM watch_state WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
        params![root_key, root_prefix, root_prefix_end],
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
        if let Some(snapshot_path) = index_snapshot_path(&state_root) {
            let _ = fs::remove_file(snapshot_path);
        }
        if let Some(manifest_path) = index_manifest_path(&state_root) {
            let _ = fs::remove_file(manifest_path);
        }
        if let Some(delta_path) = index_delta_path(&state_root) {
            let _ = fs::remove_file(delta_path);
        }
        if let Some(legacy_path) = index_state_path(&state_root, "snapshot") {
            let _ = fs::remove_file(legacy_path);
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
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let scan_threads = if root_prefers_single_thread(&root) {
        1
    } else {
        opts.threads_override.max(1)
    };
    let mut scanned_entries = scan_index_root(&root, &root_key, scan_threads)?;
    sort_and_dedup_scanned_index_entries(&mut scanned_entries);
    let fingerprint = index_fingerprint(&scanned_entries);
    let fingerprint_key = index_fingerprint_key(&root_key);
    let mut conn = open_index_db()?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let previous_fingerprint = index_fingerprint_value(&tx, &fingerprint_key)?;
    let manifest = open_index_manifest(&root_key).ok().flatten();
    let mut existing_entries = if let (Some(manifest), Some(previous_fingerprint)) =
        (manifest.as_ref(), previous_fingerprint.as_deref())
    {
        match parse_index_manifest(manifest, &root_key, previous_fingerprint) {
            Ok(entries) => entries,
            Err(_) => load_existing_index_entries(&tx, &root_key, &root_prefix, &root_prefix_end)?,
        }
    } else {
        load_existing_index_entries(&tx, &root_key, &root_prefix, &root_prefix_end)?
    };
    existing_entries
        .par_sort_unstable_by(|a, b| compare_index_entry(&a.path, a.kind, &b.path, b.kind));
    let (removed_entry_indices, added_entry_indices, updated_entry_indices) =
        diff_index_entries(&scanned_entries, &existing_entries);
    let changed_count = removed_entry_indices.len() + added_entry_indices.len();
    let incremental_limit = incremental_change_limit(existing_entries.len(), scanned_entries.len());
    if !existing_entries.is_empty() && changed_count <= incremental_limit {
        delete_index_entries(&tx, &existing_entries, &removed_entry_indices)?;
        update_index_entries_metadata(
            &tx,
            &existing_entries,
            &scanned_entries,
            &updated_entry_indices,
        )?;
        apply_index_metadata_updates(
            &mut existing_entries,
            &scanned_entries,
            &updated_entry_indices,
        );

        let mut select_string = tx
            .prepare("SELECT id FROM strings WHERE value = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_string = tx
            .prepare("INSERT INTO strings(value) VALUES (?1) RETURNING id")
            .map_err(|e| e.to_string())?;
        let mut select_dir = tx
            .prepare("SELECT id FROM dirs WHERE path = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_dir = tx
            .prepare("INSERT INTO dirs(path) VALUES (?1) RETURNING id")
            .map_err(|e| e.to_string())?;
        let mut string_ids = HashMap::new();
        let mut dir_ids = HashMap::new();
        let mut pending_entries = Vec::with_capacity(added_entry_indices.len());
        let mut added_entries = HashMap::with_capacity(added_entry_indices.len());

        for &index in &added_entry_indices {
            let scanned = &scanned_entries[index];
            let path = Path::new(&scanned.path);
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
            else {
                continue;
            };
            let Some(parent) = path.parent() else {
                continue;
            };
            let parent_key = normalize_index_dir(parent);
            let parent_id = get_or_insert_index_id(
                &mut select_dir,
                &mut insert_dir,
                &mut dir_ids,
                &parent_key,
            )?;
            let name_id = get_or_insert_index_id(
                &mut select_string,
                &mut insert_string,
                &mut string_ids,
                &name,
            )?;
            let pending = PendingIndexEntry {
                dir_id: parent_id,
                name_id,
                kind: scanned.kind,
                mtime: scanned.mtime,
                size: scanned.size,
                activity: scanned.activity,
            };
            pending_entries.push(pending);
            added_entries.insert(index, pending);
            if scanned.kind == 1 {
                get_or_insert_index_id(
                    &mut select_dir,
                    &mut insert_dir,
                    &mut dir_ids,
                    &scanned.path,
                )?;
            }
        }
        drop(select_string);
        drop(insert_string);
        drop(select_dir);
        drop(insert_dir);
        insert_index_entries(&tx, &pending_entries)?;
        tx.execute(
            "DELETE FROM dirs
             WHERE (path = ?1 OR (path >= ?2 AND path < ?3))
               AND NOT EXISTS (SELECT 1 FROM entries WHERE entries.dir_id = dirs.id)",
            params![root_key, root_prefix, root_prefix_end],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM indexed_roots WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
            params![root_key, root_prefix, root_prefix_end],
        )
        .map_err(|e| e.to_string())?;
        let refreshed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        tx.execute(
            "INSERT INTO indexed_roots(root, refreshed_at) VALUES (?1, ?2)",
            params![root_key, refreshed_at],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES (?1, ?2)",
            params![fingerprint_key, fingerprint],
        )
        .map_err(|e| e.to_string())?;
        if removed_entry_indices.is_empty() && added_entry_indices.is_empty() {
            tx.commit().map_err(|e| e.to_string())?;
            if !updated_entry_indices.is_empty() {
                write_manifest_from_existing(&root_key, &fingerprint, &existing_entries)?;
            } else if !index_sidecars_are_current(&root_key, &fingerprint) {
                rebuild_index_base_sidecars(&conn, &root_key, &fingerprint)?;
            }
            if let Some(stamp) = index_state_path(&root_key, "stamp") {
                write_stamp(&stamp);
            }
            return Ok(());
        }
        let mut delta = if let Some(previous) = previous_fingerprint.as_deref() {
            read_index_delta(&root_key, previous)?
        } else {
            BTreeMap::new()
        };
        for index in &removed_entry_indices {
            let entry = &existing_entries[*index];
            let key = (entry.path.to_string(), entry.kind);
            if delta.get(&key).is_some_and(|record| record.added) {
                delta.remove(&key);
            } else {
                delta.insert(
                    key,
                    IndexDeltaEntry {
                        added: false,
                        dir_id: entry.dir_id,
                        name_id: entry.name_id,
                        kind: entry.kind,
                        path: entry.path.to_string(),
                    },
                );
            }
        }
        for (&index, entry) in &added_entries {
            let scanned = &scanned_entries[index];
            let key = (scanned.path.clone(), scanned.kind);
            if delta.get(&key).is_some_and(|record| !record.added) {
                delta.remove(&key);
            } else {
                delta.insert(
                    key,
                    IndexDeltaEntry {
                        added: true,
                        dir_id: entry.dir_id,
                        name_id: entry.name_id,
                        kind: entry.kind,
                        path: scanned.path.clone(),
                    },
                );
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        write_incremental_manifest(
            &root_key,
            &fingerprint,
            &scanned_entries,
            &existing_entries,
            &added_entries,
        )?;
        let snapshot_exists = index_snapshot_path(&root_key).is_some_and(|path| path.is_file());
        if !snapshot_exists || delta.len() >= INDEX_DELTA_COMPACT_RECORDS {
            rebuild_index_base_sidecars(&conn, &root_key, &fingerprint)?;
        } else {
            write_index_delta(&root_key, &fingerprint, &scanned_entries, &delta)?;
        }
        if let Some(stamp) = index_state_path(&root_key, "stamp") {
            write_stamp(&stamp);
        }
        return Ok(());
    }
    if existing_entries.is_empty() && scanned_entries.len() <= 10_000 {
        tx.execute("INSERT OR IGNORE INTO dirs(path) VALUES (?1)", [&root_key])
            .map_err(|e| e.to_string())?;
        let mut pending_entries = Vec::with_capacity(scanned_entries.len());
        for scanned in &scanned_entries {
            let path = Path::new(&scanned.path);
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let Some(parent) = path.parent() else {
                continue;
            };
            let parent_key = normalize_index_dir(parent);
            tx.execute(
                "INSERT OR IGNORE INTO dirs(path) VALUES (?1)",
                [&parent_key],
            )
            .map_err(|e| e.to_string())?;
            let parent_id: i64 = tx
                .query_row("SELECT id FROM dirs WHERE path=?1", [&parent_key], |row| {
                    row.get(0)
                })
                .map_err(|e| e.to_string())?;
            tx.execute("INSERT OR IGNORE INTO strings(value) VALUES (?1)", [&name])
                .map_err(|e| e.to_string())?;
            let name_id: i64 = tx
                .query_row("SELECT id FROM strings WHERE value=?1", [&name], |row| {
                    row.get(0)
                })
                .map_err(|e| e.to_string())?;
            if scanned.kind == 1 {
                tx.execute(
                    "INSERT OR IGNORE INTO dirs(path) VALUES (?1)",
                    [&scanned.path],
                )
                .map_err(|e| e.to_string())?;
            }
            pending_entries.push(PendingIndexEntry {
                dir_id: parent_id,
                name_id,
                kind: scanned.kind,
                mtime: scanned.mtime,
                size: scanned.size,
                activity: scanned.activity,
            });
        }
        insert_index_entries(&tx, &pending_entries)?;
        let refreshed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        tx.execute(
            "INSERT OR REPLACE INTO indexed_roots(root, refreshed_at) VALUES (?1, ?2)",
            params![root_key, refreshed_at],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR REPLACE INTO index_meta(key, value) VALUES (?1, ?2)",
            params![fingerprint_key, fingerprint],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        rebuild_index_base_sidecars(&conn, &root_key, &fingerprint)?;
        if let Some(stamp) = index_state_path(&root_key, "stamp") {
            write_stamp(&stamp);
        }
        return Ok(());
    }
    drop(existing_entries);

    tx.execute_batch(
        "
        DROP INDEX IF EXISTS idx_entries_dir;
        DROP INDEX IF EXISTS idx_entries_name;
        ",
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM entries WHERE dir_id IN (
            SELECT id FROM dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)
        )",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM indexed_roots WHERE root = ?1 OR (root >= ?2 AND root < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;

    let mut string_ids = load_index_id_map(&tx, "SELECT id, value FROM strings", [])?;
    let mut dir_ids = load_index_id_map(
        &tx,
        "SELECT id, path FROM dirs
         WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
        params![root_key, root_prefix, root_prefix_end],
    )?;
    let mut next_string_id = next_index_id(&tx, "strings")?;
    let mut next_dir_id = next_index_id(&tx, "dirs")?;
    let mut insert_string = tx
        .prepare("INSERT INTO strings(id, value) VALUES (?1, ?2)")
        .map_err(|e| e.to_string())?;
    let mut insert_dir = tx
        .prepare("INSERT INTO dirs(id, path) VALUES (?1, ?2)")
        .map_err(|e| e.to_string())?;
    ensure_index_id(&mut insert_dir, &mut dir_ids, &mut next_dir_id, &root_key)?;
    let mut pending_entries = Vec::<PendingIndexEntry>::new();

    for scanned in &scanned_entries {
        let path = Path::new(&scanned.path);
        let path_key = &scanned.path;
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let Some(parent) = path.parent() else {
            continue;
        };
        let parent_key = normalize_index_dir(parent);
        let parent_id =
            ensure_index_id(&mut insert_dir, &mut dir_ids, &mut next_dir_id, &parent_key)?;
        let name_id = ensure_index_id(
            &mut insert_string,
            &mut string_ids,
            &mut next_string_id,
            &name,
        )?;
        let kind = scanned.kind;
        pending_entries.push(PendingIndexEntry {
            dir_id: parent_id,
            name_id,
            kind,
            mtime: scanned.mtime,
            size: scanned.size,
            activity: scanned.activity,
        });
        if kind == 1 {
            ensure_index_id(&mut insert_dir, &mut dir_ids, &mut next_dir_id, path_key)?;
        }
    }
    drop(insert_dir);
    drop(insert_string);
    insert_index_entries(&tx, &pending_entries)?;
    drop(dir_ids);
    drop(string_ids);
    tx.execute(
        "DELETE FROM dirs
         WHERE (path = ?1 OR (path >= ?2 AND path < ?3))
           AND NOT EXISTS (SELECT 1 FROM entries WHERE entries.dir_id = dirs.id)",
        params![root_key, root_prefix, root_prefix_end],
    )
    .map_err(|e| e.to_string())?;
    let refreshed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    tx.execute(
        "INSERT OR REPLACE INTO indexed_roots(root, refreshed_at) VALUES (?1, ?2)",
        params![root_key, refreshed_at],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT OR REPLACE INTO index_meta(key, value) VALUES (?1, ?2)",
        params![fingerprint_key, fingerprint],
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
    remove_index_delta(&root_key);
    if let Err(error) = write_index_snapshot_from_db(&conn, &root_key) {
        if let Some(snapshot_path) = index_snapshot_path(&root_key) {
            let _ = fs::remove_file(snapshot_path);
        }
        return Err(format!("failed to build index snapshot: {}", error));
    }
    write_manifest_from_scanned(&root_key, &fingerprint, &scanned_entries, &pending_entries)?;
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

fn covering_index_root(conn: &Connection, root_key: &str) -> Result<Option<String>, String> {
    let mut stmt = conn
        .prepare("SELECT root FROM indexed_roots")
        .map_err(|e| e.to_string())?;
    let roots = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut best = None;
    for root in roots {
        let root = root.map_err(|e| e.to_string())?;
        if (root == root_key
            || root == "/"
            || root_key.starts_with(&format!("{}/", root.trim_end_matches('/'))))
            && best
                .as_ref()
                .is_none_or(|current: &String| root.len() > current.len())
        {
            best = Some(root);
        }
    }
    Ok(best)
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
    let Some(mut lock) = acquire_index_refresh_lock(&lock_path) else {
        return;
    };
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
    match command.spawn() {
        Ok(child) => {
            let _ = lock.set_len(0);
            let _ = writeln!(lock, "{}", child.id());
            let _ = lock.flush();
        }
        Err(_) => {
            let _ = fs::remove_file(lock_path);
        }
    }
}

fn index_refresh_lock_owner_is_running(contents: &str) -> bool {
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return false;
    };
    let Ok(owner_exe) = fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    env::current_exe().is_ok_and(|current_exe| owner_exe == current_exe)
}

fn acquire_index_refresh_lock(lock_path: &Path) -> Option<File> {
    for _ in 0..2 {
        match File::options().write(true).create_new(true).open(lock_path) {
            Ok(mut lock) => {
                if writeln!(lock, "{}", std::process::id()).is_err() || lock.flush().is_err() {
                    let _ = fs::remove_file(lock_path);
                    return None;
                }
                return Some(lock);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let observed = fs::read_to_string(lock_path).unwrap_or_default();
                if index_refresh_lock_owner_is_running(&observed) {
                    return None;
                }
                if fs::read_to_string(lock_path).unwrap_or_default() != observed {
                    return None;
                }
                if fs::remove_file(lock_path).is_err() {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    None
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

fn recent_query_from_opts(opts: &Options) -> Result<(PathBuf, Vec<String>), String> {
    if let Some(path) = opts.path_override.as_deref() {
        return Ok((
            PathBuf::from(expand_home_path(path)),
            opts.positional.clone(),
        ));
    }
    let mut terms = opts.positional.clone();
    let root = terms
        .last()
        .filter(|token| is_implicit_content_path_token(token))
        .map(|token| PathBuf::from(expand_home_path(token)));
    if root.is_some() {
        terms.pop();
    }
    Ok((root.unwrap_or_else(|| PathBuf::from(".")), terms))
}

fn recent_refresh_threads(opts: &Options, root: &Path) -> usize {
    if opts.threads_explicit {
        return opts.threads_override;
    }
    if root_prefers_single_thread(root) {
        return 1;
    }
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(opts.threads_override)
        .clamp(1, 16)
}

fn run_recent_indexed(
    opts: &Options,
    cache: &mut DirStatsCache,
    colors: &ColorSpec,
) -> Result<SearchRun, String> {
    let limit = opts
        .recent_limit
        .ok_or_else(|| "--recent requires a positive integer".to_string())?;
    let (root_raw, terms) = recent_query_from_opts(opts)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let root_prefix = index_path_prefix(&root_key);
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let conn = open_index_db()?;
    let covering_root = covering_index_root(&conn, &root_key)?;
    let refresh_root = covering_root.as_deref().unwrap_or(&root_key).to_string();
    let live = watcher::covers_root(&conn, &root_key)?;
    drop(conn);
    if !live {
        let mut refresh_opts = opts.clone();
        refresh_opts.threads_override = recent_refresh_threads(opts, Path::new(&refresh_root));
        refresh_index_root(&refresh_root, &refresh_opts)?;
    }
    let conn = open_index_db()?;
    let fts_ready = ensure_index_search_ready(&conn).unwrap_or(false);
    let mut regexes = Vec::with_capacity(terms.len());
    for term in &terms {
        let parsed = parse_name_pattern(term, opts.regex_mode);
        regexes.push(
            RegexBuilder::new(&parsed.regex)
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid regex: {}", e))?,
        );
    }

    let stdout_is_tty = io::stdout().is_terminal();
    let use_style = style_enabled(opts, stdout_is_tty);
    let order = opts.sort_order.unwrap_or(SortOrder::Desc);
    let order_sql = match order {
        SortOrder::Asc => "ASC",
        SortOrder::Desc => "DESC",
    };
    let base_name_expr = "lower(s.value)";
    let full_path_expr =
        "lower((CASE WHEN d.path = '/' THEN '/' || s.value ELSE d.path || '/' || s.value END) || CASE WHEN e.kind = 1 THEN '/' ELSE '' END)";
    let sql_field_expr = if opts.force_full {
        full_path_expr
    } else {
        base_name_expr
    };
    let mut sql = String::from(
        "SELECT d.path, s.value, e.activity, e.size
         FROM entries e INDEXED BY idx_entries_kind_activity
         JOIN dirs d ON e.dir_id = d.id
         JOIN strings s ON e.name_id = s.id
         WHERE e.kind = ?
           AND e.activity IS NOT NULL
           AND (d.path = ? OR (d.path >= ? AND d.path < ?))",
    );
    let mut term_params = Vec::<String>::new();
    for term in &terms {
        if let Some((condition, extra_params)) = sql_prefilter_for_term(
            term,
            opts.regex_mode,
            opts.force_full,
            sql_field_expr,
            fts_ready,
        ) {
            sql.push_str(" AND (");
            sql.push_str(&condition);
            sql.push(')');
            term_params.extend(extra_params);
        }
    }
    sql.push_str(&format!(
        " ORDER BY e.activity {order_sql} LIMIT ? OFFSET ?"
    ));
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;

    let kinds: &[i64] = if opts.force_file {
        &[0]
    } else if opts.force_dir {
        &[1]
    } else {
        &[0, 1, 2]
    };
    let mut results = Vec::with_capacity(limit.saturating_mul(kinds.len()));
    let batch_size = limit.max(64).saturating_mul(4);
    for &kind in kinds {
        let mut offset = 0usize;
        let mut visible_for_kind = 0usize;
        while visible_for_kind < limit {
            let mut query_params = Vec::<rusqlite::types::Value>::with_capacity(
                6usize.saturating_add(term_params.len()),
            );
            query_params.push(kind.into());
            query_params.push(root_key.clone().into());
            query_params.push(root_prefix.clone().into());
            query_params.push(root_prefix_end.clone().into());
            query_params.extend(term_params.iter().cloned().map(Into::into));
            query_params.push(
                i64::try_from(batch_size)
                    .map_err(|_| "recent batch too large".to_string())?
                    .into(),
            );
            query_params.push(
                i64::try_from(offset)
                    .map_err(|_| "recent offset too large".to_string())?
                    .into(),
            );
            let rows = stmt
                .query_map(params_from_iter(query_params.iter()), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;

            let mut fetched = 0usize;
            for row in rows {
                let (dir_path, name, activity, size) = row.map_err(|e| e.to_string())?;
                fetched += 1;
                let mut path = if dir_path == "/" {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", dir_path, name)
                };
                if kind == 1 {
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
                    regexes.iter().all(|regex| regex.is_match(&path))
                } else {
                    regexes.iter().all(|regex| regex.is_match(base))
                };
                if !matched
                    || (opts.force_full
                        && regexes.len() > 1
                        && !regexes.iter().any(|regex| regex.is_match(base)))
                {
                    continue;
                }
                results.push(SearchResult {
                    path,
                    is_dir: kind == 1,
                    is_symlink: kind == 2,
                    metadata: None,
                    indexed_activity_nanos: activity,
                    indexed_size: size.and_then(|value| u64::try_from(value).ok()),
                });
                visible_for_kind += 1;
                if visible_for_kind >= limit {
                    break;
                }
            }
            if fetched < batch_size {
                break;
            }
            offset = offset.saturating_add(batch_size);
        }
    }
    results.sort_unstable_by(|a, b| {
        let a_activity = a.indexed_activity_nanos.unwrap_or(0);
        let b_activity = b.indexed_activity_nanos.unwrap_or(0);
        match order {
            SortOrder::Asc => a_activity
                .cmp(&b_activity)
                .then_with(|| a.is_dir.cmp(&b.is_dir))
                .then_with(|| a.path.cmp(&b.path)),
            SortOrder::Desc => b_activity
                .cmp(&a_activity)
                .then_with(|| a.is_dir.cmp(&b.is_dir))
                .then_with(|| a.path.cmp(&b.path)),
        }
    });
    results.truncate(limit);

    Ok(SearchRun {
        lines: final_transform(results, opts, use_style, stdout_is_tty, colors, cache, None),
        timed_out: false,
    })
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
    let covering_root = covering_index_root(&conn, &root_key)?;
    let refresh_root = covering_root.as_deref().unwrap_or(&root_key);
    if !watcher::covers_root(&conn, &root_key)? {
        spawn_index_refresh(refresh_root, opts, covering_root.is_none());
    }
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
    let root_prefix_end = format!("{}0", root_prefix.trim_end_matches('/'));
    let mut sql = String::from(
        "SELECT d.path, s.value, e.kind, e.mtime, e.size, e.activity
         FROM entries e
         JOIN dirs d ON e.dir_id = d.id
         JOIN strings s ON e.name_id = s.id
         WHERE (d.path = ? OR (d.path >= ? AND d.path < ?))",
    );
    let mut sql_params = vec![root_key.clone(), root_prefix.clone(), root_prefix_end];
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
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
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
        let mut emitted = 0usize;

        for row in rows {
            let (dir_path, name, kind, _mtime, _size, _activity) =
                row.map_err(|e| e.to_string())?;
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
            if opts.limit.is_some_and(|limit| emitted >= limit) {
                break;
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
            emitted += 1;
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
        let (dir_path, name, kind, mtime, size, activity) = row.map_err(|e| e.to_string())?;
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
            indexed_activity_nanos: activity.or(mtime),
            indexed_size: size.and_then(|value| u64::try_from(value).ok()),
        });
    }
    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(SearchRun {
        lines: final_transform(results, opts, use_style, stdout_is_tty, colors, cache, None),
        timed_out: false,
    })
}

fn clean_watcher_covers_search(
    opts: &Options,
    content_spec: Option<&ContainsAllSpec>,
) -> Result<bool, String> {
    let (root_raw, _) = indexed_root_from_opts(opts, content_spec)?;
    let root = fs::canonicalize(&root_raw).map_err(|e| e.to_string())?;
    let root_key = normalize_index_dir(&root);
    let conn = open_index_db()?;
    watcher::covers_root(&conn, &root_key)
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
    let tmp = path.with_extension(format!("tmp.{}", unique_temp_tag()));
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
            rows.push((None, item.path));
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
        out.push(format!(
            "{}\t{}",
            style_size(&compact, use_style),
            path_display
        ));
    }
    out
}

fn style_size(size: &str, use_style: bool) -> String {
    if use_style {
        format!("\x1b[1;96m{}\x1b[0m", size)
    } else {
        size.to_string()
    }
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
            indexed_activity_nanos: None,
            indexed_size: None,
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

fn encode_file_uri_path(path: &str) -> String {
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

fn parent_file_uri(prefix: &str, leaf_path: &str) -> String {
    let encoded_prefix = encode_file_uri_path(prefix);
    format!(
        "file://{}?select={}",
        encoded_prefix,
        encode_file_uri_path(leaf_path)
    )
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
                encode_file_uri_path(&abs_leaf),
                final_str,
            );
        } else {
            let encoded_leaf = encode_file_uri_path(&abs_leaf);
            let prefix_target = parent_file_uri(&abs_prefix, &abs_leaf);
            final_str = format!(
                "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\",
                prefix_target, prefix_colored, encoded_leaf, leaf_colored
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
        let mut lines =
            counts_summary_transform(items, stdout_is_tty, use_style, colors, opts, highlight);
        if opts.reverse {
            lines.reverse();
        }
        return lines;
    }
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
        let mut emitted = 0usize;

        for chunk in rx {
            for info in chunk {
                if opts.limit.is_some_and(|limit| emitted >= limit) {
                    continue;
                }
                if let Some(state) = cache_state.as_mut() {
                    cache_raw_record_path(&info.path.to_string_lossy(), info.is_dir, state);
                }
                let _ = lock.write_all(info.path.as_os_str().as_bytes());
                if info.is_dir && !info.path.as_os_str().as_bytes().ends_with(b"/") {
                    let _ = lock.write_all(b"/");
                }
                let _ = lock.write_all(b"\n");
                emitted += 1;
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
        rows.retain(|r| {
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
        });
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
        rows.retain(|r| {
            let base = r
                .path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("");
            basename_res.iter().any(|re| re.is_match(base))
        });
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
        rows.retain(|r| {
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
        });
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
    if let Some(root) = opts.index_snapshot.as_deref() {
        return match rebuild_index_snapshot(root) {
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
    if opts.watch_status {
        return match watcher::print_status() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        };
    }
    if opts.watch {
        return match watcher::run(&opts) {
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
    let use_watched_index = if opts.index_if_watched {
        match clean_watcher_covers_search(&opts, content_spec.as_ref()) {
            Ok(covered) => covered,
            Err(e) => {
                eprintln!("{}", e.trim());
                return ExitCode::from(1);
            }
        }
    } else {
        false
    };
    let result = if opts.recent_limit.is_some() {
        run_recent_indexed(&opts, &mut cache, &colors)
    } else if opts.index_mode || use_watched_index {
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
            limit: None,
            reverse: false,
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
            index_if_watched: false,
            index_binary: false,
            recent_limit: None,
            index_refresh: None,
            index_snapshot: None,
            index_purge: None,
            watch: false,
            watch_status: false,
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
        assert_eq!(recent_refresh_threads(&opts, Path::new("/media/disk")), 32);
    }

    #[test]
    fn recent_refresh_keeps_media_serial_by_default() {
        let opts = base_opts();
        assert_eq!(recent_refresh_threads(&opts, Path::new("/media/disk")), 1);
        assert!((1..=16).contains(&recent_refresh_threads(&opts, Path::new("/home/lewis"))));
    }

    #[test]
    fn manifest_optional_i64_round_trip() {
        for value in [None, Some(0), Some(42), Some(i64::MAX)] {
            assert_eq!(decode_optional_i64(encode_optional_i64(value)), value);
        }
    }

    #[test]
    fn sql_prefilter_skips_unrestricted_wildcards() {
        assert!(sql_prefilter_for_term("*", false, true, "path", true).is_none());
        assert!(sql_prefilter_for_term("**", false, true, "path", true).is_none());
        assert!(sql_prefilter_for_term("passwords", false, true, "path", true).is_some());
        assert!(sql_prefilter_for_term("pass*", false, true, "path", true).is_some());
    }

    #[test]
    fn scanned_index_entries_are_deduplicated_before_refresh() {
        let entry = |path: &str, kind| ScannedIndexEntry {
            path: path.to_string(),
            kind,
            mtime: Some(1),
            size: Some(2),
            activity: Some(3),
        };
        let mut entries = vec![
            entry("/root/repeated", 0),
            entry("/root/other", 1),
            entry("/root/repeated", 0),
        ];

        sort_and_dedup_scanned_index_entries(&mut entries);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/root/other");
        assert_eq!(entries[1].path, "/root/repeated");
    }

    #[test]
    fn index_batch_insert_is_idempotent_for_live_entries() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE entries (
                 id INTEGER PRIMARY KEY,
                 dir_id INTEGER NOT NULL,
                 name_id INTEGER NOT NULL,
                 kind INTEGER NOT NULL,
                 mtime INTEGER,
                 size INTEGER,
                 activity INTEGER,
                 event_kind INTEGER,
                 actor_id INTEGER,
                 UNIQUE(dir_id, name_id, kind)
             );
             INSERT INTO entries(
                 dir_id, name_id, kind, mtime, size, activity, event_kind, actor_id
             ) VALUES (10, 20, 0, 1, 2, 3, 4, 99);",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();

        insert_index_entries(
            &tx,
            &[PendingIndexEntry {
                dir_id: 10,
                name_id: 20,
                kind: 0,
                mtime: Some(11),
                size: Some(22),
                activity: Some(33),
            }],
        )
        .unwrap();
        tx.commit().unwrap();

        let row = conn
            .query_row(
                "SELECT mtime, size, activity, event_kind, actor_id FROM entries",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row, (11, 22, 33, 4, 99));
    }

    #[test]
    fn recent_query_separates_terms_from_explicit_path() {
        let mut opts = base_opts();
        opts.positional = vec!["poo".to_string(), "/home/lewis".to_string()];
        let (root, terms) = recent_query_from_opts(&opts).unwrap();
        assert_eq!(root, PathBuf::from("/home/lewis"));
        assert_eq!(terms, vec!["poo"]);

        opts.positional = vec!["poo".to_string()];
        let (root, terms) = recent_query_from_opts(&opts).unwrap();
        assert_eq!(root, PathBuf::from("."));
        assert_eq!(terms, vec!["poo"]);

        opts.path_override = Some("bare_folder".to_string());
        let (root, terms) = recent_query_from_opts(&opts).unwrap();
        assert_eq!(root, PathBuf::from("bare_folder"));
        assert_eq!(terms, vec!["poo"]);
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

    #[test]
    fn parent_file_uri_selects_files_and_directories() {
        assert_eq!(
            parent_file_uri(
                "/home/lewis/Videos/obs/",
                "/home/lewis/Videos/obs/2026-08-01 13-33-34.mp4",
            ),
            "file:///home/lewis/Videos/obs/?select=/home/lewis/Videos/obs/2026-08-01%2013-33-34.mp4"
        );
        assert_eq!(
            parent_file_uri("/home/lewis/Videos/", "/home/lewis/Videos/obs/"),
            "file:///home/lewis/Videos/?select=/home/lewis/Videos/obs/"
        );
    }

    #[test]
    fn index_diff_finds_additions_removals_and_kind_changes() {
        let scanned = vec![
            ScannedIndexEntry {
                path: "/root/added".to_string(),
                kind: 0,
                mtime: None,
                size: None,
                activity: None,
            },
            ScannedIndexEntry {
                path: "/root/changed".to_string(),
                kind: 1,
                mtime: None,
                size: None,
                activity: None,
            },
            ScannedIndexEntry {
                path: "/root/kept".to_string(),
                kind: 0,
                mtime: None,
                size: None,
                activity: None,
            },
        ];
        let existing = vec![
            ExistingIndexEntry {
                dir_id: 10,
                name_id: 20,
                path: Cow::Borrowed("/root/changed"),
                kind: 0,
                mtime: None,
                size: None,
                activity: None,
            },
            ExistingIndexEntry {
                dir_id: 11,
                name_id: 21,
                path: Cow::Borrowed("/root/kept"),
                kind: 0,
                mtime: None,
                size: None,
                activity: None,
            },
            ExistingIndexEntry {
                dir_id: 12,
                name_id: 22,
                path: Cow::Borrowed("/root/removed"),
                kind: 0,
                mtime: None,
                size: None,
                activity: None,
            },
        ];

        let (removed, added, updated) = diff_index_entries(&scanned, &existing);

        assert_eq!(removed, vec![0, 2]);
        assert_eq!(added, vec![0, 1]);
        assert!(updated.is_empty());
    }

    #[test]
    fn index_diff_detects_same_count_rename() {
        let scanned = vec![ScannedIndexEntry {
            path: "/root/new-name".to_string(),
            kind: 0,
            mtime: None,
            size: None,
            activity: None,
        }];
        let existing = vec![ExistingIndexEntry {
            dir_id: 42,
            name_id: 52,
            path: Cow::Borrowed("/root/old-name"),
            kind: 0,
            mtime: None,
            size: None,
            activity: None,
        }];

        let (removed, added, updated) = diff_index_entries(&scanned, &existing);

        assert_eq!(removed, vec![0]);
        assert_eq!(added, vec![0]);
        assert!(updated.is_empty());
    }
}
