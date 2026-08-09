# unearth - Parallel Recursive File Searcher

`unearth` is implemented in Rust.

Build:

```bash
cargo build --release
cargo build --release --features watcher --bin unearthd
```

The default release build produces the short-lived `unearth` client without watcher-only code.
Build `unearthd` with the `watcher` feature when installing the live index service.

## Source structure

`src/app.rs` is the small composition root. The responsibility boundaries are:

- `src/app/model.rs`: shared data types and time conversion helpers.
- `src/app/cli.rs`: argument parsing, help text, and binary entry-point dispatch.
- `src/app/patterns.rs`: wildcard, regex, quote, and implicit-path parsing.
- `src/app/filesystem.rs`: recursive walking, mount detection, NTFS sizing, and thread policy.
- `src/app/presentation.rs`: sizes, sorting, colors, hyperlinks, highlighting, and output transforms.
- `src/app/search.rs`: live filesystem search orchestration.
- `src/app/index/storage.rs`: SQLite, pooled-path storage, manifests, and sidecars.
- `src/app/index/refresh.rs`: scans, fingerprints, diffs, refreshes, and purge operations.
- `src/app/index/query.rs`: indexed/recent queries and result streaming.
- `src/app/index/protocol.rs`: the private `unearthd` Unix-socket protocol.
- `src/app/index/snapshot.rs`: the legacy per-query snapshot cache compatibility layer.
- `src/app/watcher.rs`: the feature-gated fanotify/inotify event state machine and metrics.
- `tests/live_watcher.sh`: temporary-tree integration and event-flood test for live updates.

The default client does not compile the watcher subsystem. `unearthd` is built with the
`watcher` feature and owns the shared live index.

## Indexed fast-start snapshots

Unearth owns the indexed database and its per-root fast-start snapshots under
`$XDG_CACHE_HOME/unearth/index` or `~/.cache/unearth/index`. Each snapshot uses
a stable FNV-1a hash of its canonical root and has a `.snapshot` suffix; the
canonical root is also embedded in the header and validated by consumers.

The versioned binary file contains the canonical root, exact counts for all
entries/files/directories, and `(kind, byte length, path)` records. The header
and first records can be read without scanning the large SQLite database, while
the remaining records are suitable for sequential background ingestion. A
sorted `.manifest` sidecar stores each current path with its pooled directory and
name IDs. Unearth memory-maps that file for incremental comparison instead of
materializing the same paths from SQLite.

`--index-refresh DIR` first performs a parallel path/kind scan and compares its
stable order-independent fingerprint with the last committed refresh. Unchanged
roots skip all SQLite and snapshot reconstruction. For a small change set, the
sorted scan is merge-diffed with the memory-mapped manifest so only removed and
added database entries are written. Unearth atomically replaces the manifest and
writes a cumulative `.delta` containing snapshot additions and tombstones; the
large base snapshot is not rewritten. A change set remains incremental while it
is at most 20% of the larger entry count, with a floor of 1,024 and a ceiling of
100,000 changes. A cumulative delta is compacted after 50,000 records. Larger
changes preserve pooled IDs, assign new IDs in memory, and insert compact entry
records in 1,000-row SQL batches as a full root reconstruction, then atomically
replace the base snapshot and manifest. Missing, stale, or malformed sidecars are
self-healed from the committed database. `--index-snapshot DIR` performs
only the snapshot-build portion for an existing index, which is useful when
upgrading. `--index-purge DIR` removes the associated snapshots. Consumers such
as Friz may use the snapshot for startup and fall back to `unearth.db` when it is
absent, incompatible, or does not cover the requested filtering mode.

## Live index

The live index uses the same pooled SQLite database; it does not create a second database or a
Friz-specific cache. Start it in the foreground with one or more roots:

```bash
unearthd /home /media
```

`unearthd` is the dedicated long-running index owner. It stays in the foreground and should be
run under a supervisor such as the user service in `systemd/unearthd.service`. The legacy
`unearth --watch ...` form remains accepted for compatibility.

When a clean watcher covers the requested root, `--full` searches automatically query the daemon's
Unix socket. If no daemon covers the root, Unearth falls back to its normal filesystem scan.

For indexed searches that request recursive directory sizes (`--sizes` or `-L`), Unearth aggregates
the stored regular-file sizes directly in SQLite while the watcher is clean. If the watcher is
dirty, absent, or any regular-file size in a requested subtree is missing, that subtree uses the
existing live filesystem walker instead, preserving size correctness during refreshes and partial
indexes.

Unearth starts the event backend before the initial scan so changes made during that scan are not
lost. It prefers fanotify file-handle events when the kernel permits filesystem marks. Without the
required permission or filesystem support it falls back to recursive inotify. Inotify needs one
watch per directory, so very large trees may require a higher `fs.inotify.max_user_watches` value;
fanotify avoids that per-directory watch cost.

The privileged fanotify path requires the kernel capabilities needed for filesystem marks and
`open_by_handle_at` (`CAP_SYS_ADMIN` and, on kernels that enforce it, `CAP_DAC_READ_SEARCH`). A
normal user invocation therefore uses inotify and keeps the SQLite database owned by that user;
running the whole command as root is not required for the fallback and can make the cache
inaccessible to later user queries.

Events are coalesced into short SQLite transactions. Current entries retain modification time,
size, activity time, event kind, and the latest actor metadata. Actor classification is deliberately
conservative: fanotify can identify the executable and UID/PID when available; inotify records the
actor as unknown because ordinary inotify events do not carry a process identity. Overflow,
unmount, unresolved file handles, renames of whole directories, and new mount points trigger a
reconciliation scan. A removed drive is not deleted from the pooled database; its rows remain
available until the mount is reattached and reconciled or the root is purged.

The watcher is event-driven by default and does not perform unconditional periodic scans. Queue
overflow, unmounts, unresolved file handles, directory moves, and new mount points still trigger
targeted reconciliation. Set `UNEARTH_WATCH_RECONCILE_SECS` to a positive number of seconds to
enable an additional periodic safety scan, or set it to `0` to explicitly disable that optional
scan. A full initial scan is still performed when the watcher starts.

Inspect persisted state with:

```bash
unearth --watch-status
```

Collect one-second resource samples for a watcher with:

```bash
unearthd --watch-metrics "$HOME/.cache/unearth/home-metrics.tsv" "$HOME"
```

The TSV contains current RSS and virtual memory, cumulative user/system CPU time, interval CPU
percentage, thread count, event counters, and scan/database timings. It is truncated when the
watcher starts and flushed after every sample, so it can be inspected while the watcher runs.

Run the repeatable live-watcher integration test against a temporary tree:

```bash
tests/live_watcher.sh
UNEARTH_LIVE_STRESS_COUNT=100000 UNEARTH_LIVE_OVERFLOW_COUNT=50000 tests/live_watcher.sh
UNEARTH_LIVE_CANCEL_COUNT=20000 tests/live_watcher.sh
```

The test verifies initial indexing, newly-created directory trees, rename and recursive removal,
the selected fanotify/inotify backend, and metrics output. If `UNEARTH_LIVE_OVERFLOW_COUNT` is
larger than the kernel inotify queue, it pauses the daemon while flooding a pre-watched directory
and verifies recorded overflow recovery. `UNEARTH_LIVE_CANCEL_COUNT` creates a large startup tree,
terminates the daemon during startup, and verifies that it exits cleanly. An unprivileged run
normally exercises the inotify fallback; fanotify selection requires the permissions available to
the daemon. Mounted drive add/remove testing should be performed separately on a disposable test
mount because it requires mount privileges and the watcher polls mount coverage rather than creating
mounts itself.

For a compact summary of a completed report:

```bash
awk -F '\t' 'NR==1 {for (i=1;i<=NF;i++) c[$i]=i; next} {n++; r=$(c["rss_bytes"]); p=$(c["cpu_percent"]); rs+=r; ps+=p; if (r>rp) rp=r; if (p>pp) pp=p; ms=$(c["elapsed_ms"]); raw=$(c["raw_events"]); b=$(c["batches"])} END {printf "samples=%d duration=%.1fs avg-rss=%.2fMiB peak-rss=%.2fMiB avg-cpu=%.3f%% peak-cpu=%.3f%% raw-events=%d batches=%d\n", n, ms/1000, rs/n/1048576, rp/1048576, ps/n, pp, raw, b}' "$HOME/.cache/unearth/home-metrics.tsv"
```

Live event updates write the pooled database directly and do not delete or rebuild binary sidecars
for every event. Indexed queries read the current pooled database immediately; an explicit refresh
or sidecar rebuild can publish a matching snapshot atomically.

Run from this repo:

```bash
./target/release/unearth --help
./target/release/unearthd --help
```

Install to your PATH:

```bash
ln -sfn "$PWD/target/release/unearth" ~/.local/bin/unearth
ln -sfn "$PWD/target/release/unearthd" ~/.local/bin/unearthd
```

Install the optional user service to start the home watcher with the user session:

```bash
mkdir -p ~/.config/systemd/user
ln -sfn "$PWD/systemd/unearthd.service" ~/.config/systemd/user/unearthd.service
systemctl --user daemon-reload
systemctl --user enable --now unearthd.service
```

The service can be inspected with `systemctl --user status unearthd` and the indexed state can be
queried independently with `unearth --watch-status`.

```
A parallel recursive file searcher

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
                       [--highlight-match|--match-red]
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
  unearth (--version|-V)

Arguments:
   <filename/dirname>:
      The file or directory name to search for. Supports exact and partial
      matching by default; use --regex/-r for regex matching.

   SEARCH MATRIX:

   Goal           | Shorthand      | Wildcard Format | Regex Format
   ---------------|----------------|-----------------|------------------
   Contains (All) | abc            | "*abc*"         | -r "abc"
   Contains (File)| abc -f         | "*abc*" -f      | -r "abc" -f
   Contains (Dir) | abc -d         | "*abc*" -d      | -r "abc" -d
   Exact (All)    | -              | -               | -r "^abc$"
   Exact (File)   | -              | -               | -r "^abc$" -f
   Exact (Dir)    | /abc/          | -               | -r "^abc$" -d
   Starts (All)   | /abc           | "abc*"          | -r "^abc"
   Starts (File)  | /abc -f        | "abc*" -f       | -r "^abc" -f
   Starts (Dir)   | /abc -d        | "abc*" -d       | -r "^abc" -d
   Ends (All)     | -              | "*abc"          | -r "abc$"
   Ends (File)    | -              | "*abc" -f       | -r "abc$" -f
   Ends (Dir)     | abc/           | "*abc" -d       | -r "abc$" -d

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
   ---------------|-----------|-----------------|------------------
   Contains       | abc       | "*abc*"         | -r "abc"
   Exact          | /abc/     | -               | -r "^abc$"
   Starts         | /abc      | "abc*"          | -r "^abc"
   Ends           | abc/      | "*abc"          | -r "abc$"

   The --full flag matches against the full absolute path instead of just
   the basename.
   It supports multiple patterns (implicit AND) and prunes redundant
   child results.

   Example: unearth --full "src" "main"   # Matches BOTH (hides children)
   Example: unearth --full "test"         # Returns /path/to/test, but hides
   /path/to/test/file

Notes:
  - Use quotes around patterns containing $ or * to prevent shell expansion.
  - Regex mode is only enabled with --regex/-r.
  - Name contains-all mode is implicit when 2+ plain positional terms are
    provided (legacy name+search_dir selector forms still use search_dir mode),
    or enabled with --contains-all:
    `unearth WORD1 WORD2 [WORD3 ...] [PATH]`
    It finds filenames/paths containing all words in any order.
    PATH is implicit only if last arg is absolute (`/x`), explicit relative
    (`./x`, `../x`, `~/x`), or contains a slash (`a/b`).
    A bare token like `folder1` is treated as a search term unless
    `--path folder1` is used.
  - Plain patterns are contains. For exact matches use regex anchors
    (e.g., --regex "^word$"), or /word/ for exact-directory shorthand.

Options:
  --dir, -d
      Limit results to directories.
  --file, -f
      Limit results to files.
  --counts
      Show a summary of matches by parent folder (folder path + count), instead
      of listing every matching file. If a directory itself matches, it counts
      as 1 match for its parent folder. Note: --long does not change --counts
      output.
  --full, -F
      Match against the full absolute path instead of just the basename.
  --classify, -C
      Force classifier decorators in output (`/`, `@`, `|`, `=`, `*`) even
      when stdout is not a TTY.
  --absolute-paths, -A
      Print absolute paths in output (display only). Does not change matching
      behavior.
  --highlight-match, --match-red
      Show matched text in red inside output paths.
  --regex, -r
      Treat filename/dirname and search_dir patterns as regular expressions.
  --long, -l
      Show the date and time of last modification and size
      (B, KiB, MiB, GiB, TiB) at the start of each line.
  --sizes
      Show compact sizes for matching files and recursively computed sizes
      for matching directories as: SIZE<TAB>PATH.
      Units are B/K/M/G/T, capped to 6 characters including the unit
      (for example 1.111M, 111.1M).
      For top-level system trees under `/` (`/mnt`, `/media`, `/dev`,
      `/proc`, `/sys`, `/run`), size is shown as `-` to avoid expensive
      recursive traversal.
      Recursive directory totals prefer an NTFS MFT fast path on ntfs/ntfs3/
      fuseblk and fall back to jwalk automatically when unavailable.
      Set UNEARTH_NTFS_DEBUG=1 to print fast-path status.
      Symlinked directories are not traversed.
  --contains-all
      Force name contains-all mode even with fewer than 3 positional words.
      Positional args are treated as required name/path terms, with an optional
      trailing PATH (implicit only for `/x`, `./x`, `../x`, `~/x`, or `a/b`).
  --path DIR
      Set a literal search root for contains-all name matching mode.
      This allows bare relative directories like `folder1` to be treated as
      path roots instead of search terms.
  -L, --long-true-dirsize
      Extended long output for directories:
      YYYY-MM-DD HH:MM:SS REALDIRSIZE FILECOUNT PATH
      Symlinked directories are not traversed (shown as link size, count 0).
  --sort FIELD ORDER
      Sort listed results by metadata. Supported:
      --sort date asc|desc, --sort size asc|desc, --sort name asc|desc
      For directories, size sort uses real allocated directory size.
      With --no-recurse/-R, size sort uses direct entry size for speed.
      Note: --counts output is always sorted ascending by count, then folder,
      and ignores --sort.
  --limit N
      Return at most N listed results. With --sort, unearth selects only the
      best N entries and sorts that subset. --counts ignores this option.
      Example: unearth "*" ~ --sort date desc --limit 10
  --reverse
      Reverse the final result order. With --limit, the limit is selected
      before the selected results are reversed.
  --hyperlink
      Emit split file:// hyperlinks. The parent-directory link includes
      ?select= so PCManFM can preselect the matching file or directory.
  --no-recurse, -R
      Search only the immediate entries in each search root (no recursion).
  --follow-links
      Follow symlinked directories while searching.
  --ignore
      Respect ignore rules (.gitignore/.ignore/.fdignore). By default, unearth
      bypasses ignore rules.
  --visible-only
      Exclude hidden files/directories (dotfiles). By default, unearth includes
      hidden entries.
  --threads N
      Set worker thread count for unearth and directory size calculations.
      Must be a positive integer. Default: 8.
  --cache-raw
      Save matched directories to:
      /tmp/fzf-history-$USER/universal-last-dirs-<fish pid>
      and files to:
      /tmp/fzf-history-$USER/universal-last-files-<fish pid>
      For every match, also save its parent directory to the dirs file.
  --snapshot-cache
      Print the last complete cached snapshot for this exact command
      immediately, then refresh that snapshot in the background.
      Timed-out scans do not replace an existing snapshot. Background
      refreshes use a long timeout by default unless --timeout is passed
      explicitly.
  --index
      Query the global pooled path database instead of walking the filesystem.
      The database is stored at ~/.cache/unearth/index/unearth.db unless
      XDG_CACHE_HOME is set. Current DB rows are returned immediately; if the
      root is missing or stale, one background refresh is started. Plain terms
      of three or more characters use trigram indexes over pooled names and
      directory paths. Existing databases build these indexes once on the first
      indexed query after upgrading, which increases that same database's size.
  --watch ROOT ...
      Compatibility alias for the separate unearthd daemon. Perform an initial
      scan and continuously update the pooled database from fanotify filesystem
      or recursive inotify events.
  --watch-status
      Print persisted watcher backend, state, generation, and recovery status.
  --watch-metrics FILE
      Sample the live watcher once per second and write a TSV report containing
      RSS/virtual memory, user/system CPU time, CPU percentage, thread count,
      event throughput, and refresh/database timings. This is valid only with
      --watch; the file is truncated when the watcher starts.
  --index-if-watched
      Query the pooled database when a clean live watcher covers the requested
      root; otherwise use the normal filesystem scan.
  --recent N
      Query the indexed database for the N most recently created-or-modified
      entries under DIR, ordered newest first. If DIR is omitted, '.' is used.
      This uses the same unearth index as --index and synchronously refreshes the
      covering indexed root unless a clean --watch owner covers it, so results
      never come from an intentionally stale snapshot. Files, directories, and symlinks are included by default; use
      -f or -d to restrict the type. Add --long/-l to show the activity
      date and size beside each path. Terms before DIR filter the results;
      --full/-F matches those terms against the complete path. Filtering is
      performed in SQL before matching rows are rendered.
  --index-refresh DIR
      Rebuild indexed rows for DIR in the global database. Directory paths and
      repeated entry names are stored once and entries link to them by integer
      IDs. Entry metadata stores mtime, size, and a recent-activity timestamp
      based on create/modify time for fast --recent queries.
      Small change sets are merge-diffed through a memory-mapped manifest and
      published as cumulative snapshot deltas; large changes use in-memory ID
      assignment and batched full reconstruction. Sidecars are atomically
      replaced after the database transaction commits.
  --index-snapshot DIR
      Rebuild only the fast-start binary snapshot from existing indexed rows.
      This does not walk the filesystem or change the indexed database rows.
  --index-purge DIR
      Remove indexed rows for DIR and all indexed children from the global
      database and delete their fast-start snapshots. Existing roots are
      canonicalized; missing roots are normalized lexically, so stale rows can
      be removed after a drive is disconnected.
  --timeout N
      Per-invocation timeout for each unearth call. Default: 6s
      Examples: --timeout 10, --timeout 10s, --timeout 2m
  --bypass, -b
      Force treating the search_dir as a pattern, even if it exists as
      a directory.
  --version, -V
      Show version and exit.
```
