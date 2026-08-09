use super::filesystem::effective_threads_override;
use super::index::{
    clean_watcher_covers_search, print_watch_status, purge_index_root, purge_index_root_path,
    rebuild_index_snapshot, rebuild_index_snapshot_path, refresh_index_root,
    refresh_index_root_path, run_indexed, run_recent_indexed, snapshot_cache_path,
    snapshot_lock_path, spawn_snapshot_refresh, stream_snapshot_cache, write_snapshot_cache,
};
use super::model::{ColorWhen, DirStatsCache, Options, SortField, SortOrder};
use super::patterns::contains_all_spec_from_opts;
use super::presentation::{default_color_spec, parse_ls_colors};
use super::search::{run_contains_all, run_full, run_standard};
#[cfg(feature = "watcher")]
use super::watcher;
use super::{SNAPSHOT_REFRESH_TIMEOUT, VERSION};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufWriter, IsTerminal, Write};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(not(feature = "watcher"))]
use std::process::Command;
use std::process::ExitCode;
use std::time::Duration;

fn is_broken_pipe_message(error: &str) -> bool {
    error.to_ascii_lowercase().contains("broken pipe")
}
pub(crate) fn usage() -> String {
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
                       [--watch ROOT ...] [--watch-status] [--watch-metrics FILE]
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
    ~/.cache/fsx/index/fsx.db instead of walking the filesystem. It
    returns current DB rows immediately and starts one background refresh when
    the root is missing or stale, unless a clean --watch owner covers it.
  - --index-if-watched queries that database only when a clean live watcher
    covers the requested search root. Otherwise it performs the normal live
    filesystem scan. This is intended for shell functions that need an
    automatic indexed-or-scan fallback.
    Recursive directory sizes use SQL aggregation when that clean index has a
    stored size for every regular file in the subtree; incomplete subtrees
    fall back to the live filesystem size walker.
    Full-path (-F) searches use the same fallback automatically when a clean
    watcher covers the root; --index-if-watched remains available explicitly.
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
  - --watch ROOT ... is retained as a compatibility alias for starting the live index owner.
    Prefer the separate `fsxd` executable. It performs
    an initial scan, then batches create/modify/delete/rename events into the same pooled SQLite
    database. fanotify is preferred when the kernel and permissions support filesystem file
    handles; inotify is used as the recursive fallback. The owner is exclusive per root; a new
    invocation asks an existing owner for graceful shutdown before replacing it. It records a
    boot ID, process start time, and heartbeat. Queue overflow, unmounts, unsupported
    event resolution, and new mount points trigger a scoped reconciliation scan instead of
    silently losing entries. Periodic safety scans are disabled by default; set
    UNEARTH_WATCH_RECONCILE_SECS to a positive number to enable them. This command stays in
    the foreground until interrupted. Starting a watcher for an already watched root asks the
    existing owner to stop and replaces it with the new options.
  - --watch-status prints live watcher state recorded in the pooled database and exits.
    It marks a state stopped when the recorded watcher process is no longer alive.
  - --watch-metrics FILE samples the watch process once per second and writes a TSV report,
    truncating it at startup. It contains current RSS/virtual memory, user/system CPU time,
    CPU percentage, thread count, event throughput, and refresh/database timings.
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

pub(crate) fn parse_duration(t: &str) -> Result<Duration, String> {
    let (number, multiplier) = match t.strip_suffix(['s', 'm']) {
        Some(value) => (value, t.as_bytes().last().copied()),
        None => (t, None),
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("Invalid timeout format: {}", t));
    }
    let seconds = number
        .parse::<u64>()
        .map_err(|_| format!("Invalid timeout format: {}", t))?;
    let seconds = if multiplier == Some(b'm') {
        seconds
            .checked_mul(60)
            .ok_or_else(|| format!("Invalid timeout format: {}", t))?
    } else {
        seconds
    };
    Ok(Duration::from_secs(seconds))
}

pub(crate) fn parse_args() -> Result<Options, String> {
    // `args()` panics before we can report an error when a Unix operand is not
    // valid UTF-8. Keep parsing total; filesystem paths are handled as bytes
    // by the search layer where possible.
    let raw_args: Vec<OsString> = env::args_os().skip(1).collect();
    let mut options = parse_args_from(
        raw_args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    )?;
    let (path_override_os, positional_os) = raw_operands(&raw_args)?;
    options.path_override_os = path_override_os;
    options.positional_os = positional_os;
    options.index_refresh_os = raw_option_value(&raw_args, "--index-refresh");
    options.index_snapshot_os = raw_option_value(&raw_args, "--index-snapshot");
    options.index_purge_os = raw_option_value(&raw_args, "--index-purge");
    options.watch_metrics_os = raw_option_value(&raw_args, "--watch-metrics");
    Ok(options)
}

fn raw_option_value(args: &[OsString], option: &str) -> Option<OsString> {
    args.iter().enumerate().find_map(|(index, value)| {
        if let Some(value) = inline_option_value(value, option) {
            Some(value)
        } else if value.to_str() == Some(option) {
            args.get(index + 1).cloned()
        } else {
            None
        }
    })
}

fn inline_option_value(value: &OsString, option: &str) -> Option<OsString> {
    #[cfg(unix)]
    {
        let prefix = format!("{option}=");
        let bytes = value.as_os_str().as_bytes();
        return bytes
            .strip_prefix(prefix.as_bytes())
            .map(|value| OsString::from_vec(value.to_vec()));
    }
    #[cfg(not(unix))]
    {
        value
            .to_str()
            .and_then(|value| value.strip_prefix(&format!("{option}=")))
            .map(OsString::from)
    }
}

fn raw_operands(args: &[OsString]) -> Result<(Option<OsString>, Vec<OsString>), String> {
    let mut positional = Vec::new();
    let mut path_override = None;
    let mut i = 0usize;
    let mut options_done = false;
    while i < args.len() {
        let raw = args[i].to_str();
        if options_done {
            positional.push(args[i].clone());
            i += 1;
            continue;
        }
        if raw == Some("--") {
            options_done = true;
            i += 1;
            continue;
        }
        if let Some(value) = inline_option_value(&args[i], "--path") {
            path_override = Some(value);
            i += 1;
            continue;
        }
        if [
            "--timeout",
            "--threads",
            "--color",
            "--sort",
            "--limit",
            "--recent",
            "--index-refresh",
            "--index-snapshot",
            "--index-purge",
            "--watch-metrics",
        ]
        .iter()
        .any(|option| inline_option_value(&args[i], option).is_some())
        {
            i += 1;
            continue;
        }
        match raw {
            Some("--path") => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| "--path requires a directory argument".to_string())?;
                path_override = Some(value.clone());
            }
            Some(value) if value.starts_with("--path=") => {
                path_override = Some(OsString::from(&value[7..]));
            }
            Some(value)
                if matches!(
                    value,
                    "--timeout"
                        | "--threads"
                        | "--color"
                        | "--sort"
                        | "--limit"
                        | "--recent"
                        | "--index-refresh"
                        | "--index-snapshot"
                        | "--index-purge"
                        | "--watch-metrics"
                ) =>
            {
                i += if value == "--sort" { 2 } else { 1 };
                if i >= args.len() {
                    return Err(format!("{value} requires an argument"));
                }
            }
            Some(value) if value.starts_with("--") => {}
            Some(value) if value.starts_with('-') && value.len() > 1 => {}
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    Ok((path_override, positional))
}

pub(crate) fn parse_args_from<I>(arguments: I) -> Result<Options, String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = arguments.into_iter().collect();
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
        watch_metrics: None,
        watch_metrics_os: None,
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
        path_override_os: None,
        positional_os: Vec::new(),
        index_refresh_os: None,
        index_snapshot_os: None,
        index_purge_os: None,
    };

    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];

        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            if !arg[1..].chars().all(|ch| {
                matches!(
                    ch,
                    'd' | 'f' | 'F' | 'C' | 'A' | 'r' | 'R' | 'H' | 'b' | 'l' | 'L'
                )
            }) {
                return Err(format!(
                    "Unknown short option '{}'; use --help for usage",
                    arg
                ));
            }
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
                    _ => unreachable!("short options were validated above"),
                };
                debug_assert!(handled);
            }
            i += 1;
            continue;
        }

        match arg.as_str() {
            "--timeout" => {
                i += 1;
                if i >= args.len() {
                    return Err("--timeout requires a duration".to_string());
                }
                opts.timeout_dur = parse_duration(&args[i])?;
                opts.timeout_explicit = true;
            }
            _ if arg.starts_with("--timeout=") => {
                opts.timeout_dur = parse_duration(arg.trim_start_matches("--timeout="))?;
                opts.timeout_explicit = true;
            }
            "--threads" => {
                i += 1;
                if i >= args.len() {
                    return Err("--threads requires a positive integer".to_string());
                }
                opts.threads_override = args[i]
                    .parse::<usize>()
                    .map_err(|_| "Invalid threads count")?;
                if opts.threads_override == 0 {
                    return Err("--threads requires a positive integer".to_string());
                }
                opts.threads_explicit = true;
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
                if i >= args.len() {
                    return Err("--color requires auto, always, or never".to_string());
                }
                opts.color_when = parse_color_when(&args[i])?;
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
                if i + 2 >= args.len() {
                    return Err("--sort requires a field and order".to_string());
                }
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
            "--watch-metrics" => {
                i += 1;
                if i < args.len() {
                    opts.watch_metrics = Some(args[i].clone());
                } else {
                    return Err("--watch-metrics requires a report file path".to_string());
                }
            }
            _ if arg.starts_with("--watch-metrics=") => {
                let value = arg.trim_start_matches("--watch-metrics=");
                if value.is_empty() {
                    return Err("--watch-metrics requires a non-empty report file path".to_string());
                }
                opts.watch_metrics = Some(value.to_string());
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
            _ if arg.starts_with('-') => {
                return Err(format!("Unknown option '{}'; use --help for usage", arg));
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
    if opts.watch_metrics.is_some() && !opts.watch {
        return Err("--watch-metrics requires --watch".to_string());
    }
    if opts.force_dir && opts.force_file {
        return Err("--dir and --file are mutually exclusive".to_string());
    }
    if opts.index_binary
        && (opts.long_format
            || opts.sizes
            || opts.counts
            || opts.sort_field.is_some()
            || opts.reverse
            || opts.classify
            || opts.hyperlinks
            || opts.highlight_match
            || opts.absolute_paths
            || opts.color_when == ColorWhen::Always)
    {
        return Err("--index-binary cannot be combined with formatted output options".to_string());
    }
    Ok(opts)
}

pub(crate) fn parse_color_when(v: &str) -> Result<ColorWhen, String> {
    match v {
        "auto" => Ok(ColorWhen::Auto),
        "always" => Ok(ColorWhen::Always),
        "never" => Ok(ColorWhen::Never),
        _ => Err(format!("Unsupported --color value '{}'", v)),
    }
}

pub(crate) fn cli_main() -> ExitCode {
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
        let result = opts
            .index_refresh_os
            .as_deref()
            .map(std::path::Path::new)
            .map(|path| refresh_index_root_path(path, &opts))
            .unwrap_or_else(|| refresh_index_root(root, &opts));
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        };
    }
    if let Some(root) = opts.index_snapshot.as_deref() {
        let result = opts
            .index_snapshot_os
            .as_deref()
            .map(std::path::Path::new)
            .map(rebuild_index_snapshot_path)
            .unwrap_or_else(|| rebuild_index_snapshot(root));
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        };
    }
    if let Some(root) = opts.index_purge.as_deref() {
        let result = opts
            .index_purge_os
            .as_deref()
            .map(std::path::Path::new)
            .map(purge_index_root_path)
            .unwrap_or_else(|| purge_index_root(root));
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        };
    }
    if opts.watch_status {
        return match print_watch_status() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", e);
                ExitCode::from(1)
            }
        };
    }
    if opts.watch {
        return run_watch(&opts);
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
                    if e.kind() == io::ErrorKind::BrokenPipe {
                        return ExitCode::SUCCESS;
                    }
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
    let stdout_is_tty = io::stdout().is_terminal();
    let colors = if !super::presentation::style_enabled(&opts, stdout_is_tty) {
        default_color_spec()
    } else {
        parse_ls_colors()
    };
    let implicit_watched_index = opts.force_full
        && !opts.index_mode
        && opts.recent_limit.is_none()
        && !opts.snapshot_cache
        && !opts.snapshot_refresh
        && content_spec.is_none();
    let use_watched_index = if opts.index_if_watched || implicit_watched_index {
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
                    if let Err(error) = lock
                        .write_all(line.as_bytes())
                        .and_then(|_| lock.write_all(b"\n"))
                    {
                        if error.kind() == io::ErrorKind::BrokenPipe {
                            return ExitCode::SUCCESS;
                        }
                        eprintln!("unearth: failed to write output: {error}");
                        return ExitCode::from(1);
                    }
                }
                if let Err(error) = lock.flush() {
                    if error.kind() == io::ErrorKind::BrokenPipe {
                        return ExitCode::SUCCESS;
                    }
                    eprintln!("unearth: failed to flush output: {error}");
                    return ExitCode::from(1);
                }
            }
            if run.timed_out {
                eprintln!("unearth: search timed out; results are incomplete");
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            if is_broken_pipe_message(&e) {
                return ExitCode::SUCCESS;
            }
            if !e.trim().is_empty() {
                eprintln!("{}", e.trim());
            }
            ExitCode::from(1)
        }
    }
}

#[cfg(feature = "watcher")]
pub(crate) fn run_watch(opts: &Options) -> ExitCode {
    match watcher::run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error);
            ExitCode::from(1)
        }
    }
}

#[cfg(not(feature = "watcher"))]
pub(crate) fn run_compat_watch() -> ExitCode {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    let daemon = env::var_os("FSX_DAEMON_BIN").unwrap_or_else(|| "fsxd".into());
    match Command::new(&daemon).args(&args).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(error) if error.kind() == io::ErrorKind::NotFound && daemon != "unearthd" => {
            match Command::new("unearthd").args(&args).status() {
                Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
                Err(fallback_error) => {
                    eprintln!(
                        "unearth: cannot start fsxd or compatibility unearthd: {}",
                        fallback_error
                    );
                    ExitCode::from(1)
                }
            }
        }
        Err(error) => {
            eprintln!("unearth: cannot start fsxd: {}", error);
            ExitCode::from(1)
        }
    }
}

#[cfg(not(feature = "watcher"))]
pub(crate) fn run_watch(_opts: &Options) -> ExitCode {
    run_compat_watch()
}

pub(crate) fn daemon_usage() -> &'static str {
    "fsx live filesystem index daemon\n\nUsage:\n  fsxd [--threads N] [--watch-metrics FILE] ROOT ...\n\nOptions:\n  --threads N             Metadata worker count (default: 8)\n  --watch-metrics FILE   Write one-second resource metrics to FILE\n  --help                 Show this help\n  --version              Show the version\n\nThe daemon owns the shared fsx SQLite index and stays in the foreground. Run\nit under systemd or another supervisor for automatic restart. unearthd is\nretained as a compatibility entry point.\n"
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn inline_non_utf8_path_option_keeps_original_bytes() {
        let value = OsString::from_vec(b"--path=/tmp/raw-\xff".to_vec());
        let decoded = inline_option_value(&value, "--path").expect("inline option value");
        assert_eq!(decoded.as_os_str().as_bytes(), b"/tmp/raw-\xff");

        let (path, positional) = raw_operands(&[value]).expect("raw operands");
        assert_eq!(
            path.expect("path override").as_os_str().as_bytes(),
            b"/tmp/raw-\xff"
        );
        assert!(positional.is_empty());
    }
}

pub(crate) fn fsxd_main() -> ExitCode {
    let raw_args: Vec<OsString> = env::args_os().skip(1).collect();
    if raw_args
        .iter()
        .any(|arg| arg == OsStr::new("--help") || arg == OsStr::new("-h"))
    {
        print!("{}", daemon_usage());
        return ExitCode::SUCCESS;
    }
    if raw_args
        .iter()
        .any(|arg| arg == OsStr::new("--version") || arg == OsStr::new("-V"))
    {
        println!("fsxd {}", VERSION);
        return ExitCode::SUCCESS;
    }
    let mut arguments = Vec::with_capacity(raw_args.len() + 1);
    arguments.push(OsString::from("--watch"));
    arguments.extend(raw_args);
    let mut opts = match parse_args_from(
        arguments
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    ) {
        Ok(opts) => opts,
        Err(error) => {
            if !error.is_empty() {
                eprintln!("{}", error);
            }
            return ExitCode::from(2);
        }
    };
    if let Ok((path_override_os, positional_os)) = raw_operands(&arguments) {
        opts.path_override_os = path_override_os;
        opts.positional_os = positional_os;
    }
    opts.watch_metrics_os = raw_option_value(&arguments, "--watch-metrics");
    run_fsxd(&opts)
}

#[cfg(feature = "watcher")]
pub(crate) fn run_fsxd(opts: &Options) -> ExitCode {
    match watcher::run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error);
            ExitCode::from(1)
        }
    }
}

#[cfg(not(feature = "watcher"))]
pub(crate) fn run_fsxd(_opts: &Options) -> ExitCode {
    eprintln!("fsxd was built without the watcher feature");
    ExitCode::from(1)
}
