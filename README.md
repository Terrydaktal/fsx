# unearth - Parallel Recursive File Searcher

`unearth` is implemented in Rust.

Build:

```bash
cargo build --release
```

Run from this repo:

```bash
./target/release/unearth --help
```

Install to your PATH:

```bash
install -Dm755 ./target/release/unearth ~/.local/bin/unearth
```

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
                       [--timeout N] [--sort date|size|name asc|desc]
                       [--no-recurse|-R] [--follow-links]
                       [--ignore] [--hidden|-H] [--threads N]
                       [--cache-raw] [--snapshot-cache]
                       [--index] [--index-refresh DIR]
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
      root is missing or stale, one background refresh is started.
  --index-refresh DIR
      Rebuild indexed rows for DIR in the global database. Directory paths and
      repeated entry names are stored once and entries link to them by integer
      IDs.
  --timeout N
      Per-invocation timeout for each unearth call. Default: 6s
      Examples: --timeout 10, --timeout 10s, --timeout 2m
  --bypass, -b
      Force treating the search_dir as a pattern, even if it exists as
      a directory.
  --version, -V
      Show version and exit.
```
