# twig

`twig` is a single-binary Rust CLI that lists one directory level (`max_depth = 1`) with optional long-format metadata, sorting, Git integration, symlink target rendering, hyperlink support, path caching for shell tooling, and NTFS-aware recursive stats fast paths.

## Justification

1. Eligible plain and long listings use dedicated `std::fs::read_dir` fast paths. When piping or when there are >1000 entries in the listing, `--color=auto` and `--hyperlink=auto` do not apply colour or hyperlinks.
   - Benchmarking with hyperlinks and colours disabled on all `twig` is **2–3× faster** than `/bin/ls` (with `twig -la` same speed as `/bin/ls -la`) and **8–12× faster** than `eza` (with `twig -la` **1.5× faster** than `eza -la`).  
   - With hyperlinks and colour forced on both, `twig` is **4× faster** than `eza` with hyperlinks and colour (with `twig -la` **2.2× faster** than `eza -la`).
2. On NTFS-like mounts, recursive stats (`-S`, `-c`, `--sort dircount|filecount`) attempt an MFT-based fast path first, then automatically fall back to regular filesystem scanning when unavailable.
3. `--cache-raw` writes full directory/file path lists to `/tmp/fzf-history-$USER/universal-last-{dirs,files}-<fish_pid>`, allowing quick access to listed files with a fuzzy picker.
4. `-c`, `--counts` – recursive directory/file count columns. Supports `--sort dircount` and `--sort filecount`.
5. `eza` `--git-repos` and `--git` are combined into a smart `--git` flag that shows either or both columns when relevant
6. In `-X` / `--absolute`, `twig` splits the prefix and basename into separate hyperlinks, with the prefix styled in white.
7. `-x`, `--show-targets` – explicit flag to display symlink targets (usable outside long mode).
8. Symlink targets are rendered/styled separately and can be hyperlinked independently. The hyperlink is also split; the prefix is coloured white and styled separately from the `LS_COLORS` scheme.
9. Column order follows flag order from `argv`, including compact short bundles. `-l` is expanded into ordered `p,s,o,t` at parse time, so later flags append after it.
10. `--header` moves to the bottom when `-r` (reverse) is used.
11. `-L` – one file per line view. `twig` force‑enables list mode when piped, when `--header` is used, or when any detail columns are active.
12. `-a -S` shows `.` (not `..`) and gives `.` full recursive true size.
13. `-s` in `twig` shows logical size of files and allocated blocks for directories and `-S` shows allocated block size of files and true recursive sizes of directories.
14. `-H`, `--no-dedupe-hardlinks` – toggle for `-S` hardlink deduplication.
15. Recursive totals include hidden descendants even when hidden entries are not displayed.
16. `--git-fetch` fetches the current repository when inside one, plus immediate child repository roots.

## Project Structure

```text
.
├── .cargo/
│   └── config.toml      # local cargo build flags (target-cpu=native)
├── Cargo.toml          # crate metadata and dependencies
├── Cargo.lock          # locked dependency graph
├── src/
│   ├── main.rs         # process entry point and module wiring
│   ├── app.rs          # listing orchestration, sorting, entry construction and cache coordination
│   ├── cli.rs          # Clap options, flag-order semantics and resolved display context
│   ├── fs_ops.rs       # metadata, recursive size/count collection and NTFS/MFT paths
│   ├── git.rs          # Git status, repository markers, remote state and fetch operations
│   ├── model.rs        # shared entry and filesystem data structures
│   └── render.rs       # fast paths, detailed/grid output, styling and hyperlinks
└── target/             # build artifacts (ignored in git)
```

### File Responsibilities

- `src/main.rs`
  - Starts the process and delegates to the application module.
- `src/app.rs`
  - Collects one or multiple listing paths, sorts entries, builds display records,
    coordinates Git metadata, and emits output.
- `src/cli.rs`
  - Defines Clap flags and preserves flag-order behavior for column order and
    implicit sorting.
- `src/fs_ops.rs`
  - Reads metadata, computes recursive sizes/counts, and selects the NTFS MFT
    path or filesystem walker.
- `src/git.rs`
  - Computes file status, repository markers, remote state, and `--git-fetch`
    behavior.
- `src/model.rs`
  - Contains shared entry and mount data structures passed between modules.
- `src/render.rs`
  - Owns the large-directory fast paths, list/grid rendering, LS_COLORS,
    hyperlinks, symlink targets, permissions, sizes, and raw path caches.
- `.cargo/config.toml`
  - Enables `-C target-cpu=native` for local optimized builds

## Build, Run, Install

### Build (release)

```bash
cargo build --release
```

Binary:

```text
./target/release/twig
```

### Run

```bash
./target/release/twig [OPTIONS] [PATH]
```

Default path is `.`.

## CLI Summary

From `twig --help`:

- `-a, --all` list all files, including hidden
  - with `-S`, `.` is shown but `..` is omitted
  - with implicit/default sort, injected dot entries are pinned to top; with explicit `--sort`, they are sorted normally
- `-A, --almost-all` list hidden but exclude `.` and `..`
- `-l, --long` shorthand for `-Lptos --show-targets`
- `-L, --list` force one-entry-per-line list mode
- `-d, --dirs-only` show only directories in the listed folder
- `-n, --no-traverse` list a directory entry itself (do not list its contents)
- `-p, --permissions` show permission bits
- `-s, --size` show logical file size and allocated dir size
- `-c, --counts` show recursive dir/file counts for directory entries; auto-sorts by total files + dirs ascending
- `-o, --owner` show file owner
- `-g, --group` show group
- `-t, --modified` show mtime
- `-F, --classify` append classifier (`/`, `@`, `*`, etc.)
- `--sort <name|type|time|size|dircount|filecount>` sort key (default `type`; `date` aliases `time`)
- `-r, --reverse` reverse listing order
- `-U, --hyperlink[=<always|auto|never>]` render names as OSC8 hyperlinks
- `-x, --show-targets` show symlink target paths
- `-X, --absolute` show absolute paths in output
- `-G, --git` smart Git columns:
  - file staged/unstaged status when listing path is in a Git repo
  - repo-root status markers when listed entries include Git repo roots
- `-1, --dereference` use symlink target size/time fields for `-s`/`-S`/`-t`
- `-S, --true-size` show allocated file size + recursive allocated dir size; auto-sorts by size ascending
- `-H, --no-dedupe-hardlinks` disable hardlink dedupe for `-S`
- `-f, --git-fetch` fetch the current and immediately nested Git repositories
- `--header` show list headers (moved to bottom with `-r`)
- `--color <always|auto|never>` control ANSI color rendering
- `--cache-raw` write listed full paths for dirs/files to `/tmp/fzf-history-$USER/...`

## Operation Pipeline (Execution Order)

For each invocation, `twig` runs roughly this pipeline:

1. Parse CLI flags and build rendering context.
2. Optionally precompute recursive stats when needed:
   - recursive sizes for `-S`
   - recursive counts for `-c` and `--sort dircount|filecount`
   - root recursive total for injected `.` in `-a -S`
   - on NTFS-like mounts, attempts MFT scan first and falls back automatically
3. Scan one directory level for displayed entries (`std::fs::read_dir`).
4. Build per-entry metadata struct:
   - file type
   - symlink target/broken state
   - size string + numeric sort size
   - owner/group/time strings
5. Resolve `--color` and `--hyperlink` modes:
   - `always`: always enabled
   - `never`: always disabled
   - `auto`: enabled only on TTY and only when shown entry count is `<= 1000`
6. Sort entries by selected key, apply reverse if requested.
7. Optionally populate Git columns:
   - file status pair when listing path is inside a Git repo
   - repo-root cleanliness marker when listed entries include Git repo roots
8. Optionally write raw path cache files (`--cache-raw`).
9. Render all rows into one buffered `String`.
10. Single `stdout.lock().write_all(...)` write.

## NTFS Fast Path (`-S`, `-c`, `--sort dircount|filecount`)

When the listing path is on `ntfs`, `ntfs3`, or `fuseblk`:

1. `twig` tries an MFT-based recursive scan via the underlying block device.
2. If that is unavailable (for example permission/device access), `twig` falls back to normal filesystem recursion.

Environment controls:

- `TWIG_NTFS_THREADS=<n>`: overrides NTFS recursive scanner worker count.
- `TWIG_NTFS_DEBUG=1`: prints whether MFT fast path was enabled or unavailable.

## Size Semantics

`twig` intentionally separates logical size and on-disk size:

- `-s` / `--size`
  - Files: logical byte size (`metadata.len()`)
  - Dirs: allocated blocks for that directory entry (`st_blocks * 512`)
  - With `-d` on symlink entries:
    - symlink -> file: logical target file size
    - symlink -> dir: allocated blocks of target directory

- `-S` / `--true-size`
  - Files: allocated blocks (`st_blocks * 512`)
  - Dirs: recursive allocated total (directory + descendants)
  - In `-a -S`, injected `.` shows the listing directory's full recursive total
  - With `-d` on symlink entries:
    - symlink -> file: allocated blocks of target file
    - symlink -> dir: recursive allocated total of target directory

### Hardlink Deduplication (`-S` mode)

By default, `-S` deduplicates hardlinks by `(dev, ino)` while aggregating descendants.

- Default: dedupe on
- `-H` / `--no-dedupe-hardlinks`: dedupe off

## Git Integration

### `--git` two-character file status

Column format is:
- left: staged/index state
- right: unstaged/worktree state

Status symbols:
- `-` unmodified / no change in that side
- `M` modified
- `A` added to index
- `N` new untracked (worktree)
- `D` deleted
- `R` renamed
- `T` type-change
- `I` ignored
- `U` unmerged/conflicted

Common combinations:
- `MM` staged + unstaged modifications
- `M-` staged modified only
- `-M` unstaged modified only
- `A-` staged new file
- `AM` staged new file plus unstaged edits
- `D-` staged deletion
- `R-` staged rename
- `UU` conflict

Color mapping:
- `N`/`A`: green
- `M`/`R`/`T`: yellow
- `D`/`U`: red
- `-`/`I`: dimmed

### Repo-root status (under `--git`)

Shown only for directory entries that are Git roots:
- `|` clean repo (green)
- `+` dirty repo (red)
- `~` unknown status (yellow)

## Symlink Behavior

- Classifier for symlink entries uses `@`.
- Long mode prints `name@ -> target`.
- Target rendering:
  - Prefix path segment before basename is forced white (`255,255,255`)
  - Basename uses `LS_COLORS`
  - If `-F` and target is a directory, target suffix `/` is shown
- Broken symlink rows are highlighted with red background and white foreground.
- Hyperlink mode can hyperlink link name and target path independently.

## LS_COLORS Integration

- Entry names are styled from `LS_COLORS`.
- Directory type marker `d` in permissions now uses the same directory style as name rendering.
- Symlink targets also resolve color via `LS_COLORS` where metadata is available.

## Color/Hyperlink Auto Guardrail

For `--color=auto` and `--hyperlink=auto` (including plain `-U`):
- output is disabled when stdout is not a TTY
- output is disabled when shown entry count is greater than `1000` (files or dirs)
- `always` bypasses these guards

## Raw Cache Output (`--cache-raw`)

When enabled, `twig` writes full absolute paths of displayed entries to:
- Paths are lexically normalized (for example, `/home/user/./file` is written as `/home/user/file`)

- Dirs:
  - `/tmp/fzf-history-$USER/universal-last-dirs-<fish_pid>`
- Files:
  - `/tmp/fzf-history-$USER/universal-last-files-<fish_pid>`

PID suffix resolution order:
1. `fish_pid` environment variable
2. parsed parent PID from `/proc/self/stat`
3. current process PID fallback

## Output Modes

- Compact mode (default): names separated by two spaces, single row
- Detailed mode: one row per entry when any of:
  - `-L`, `-p`, `-s`, `-c`, `-o`, `-g`, `-t`, `-l`, `--git`, `-S`, `--header`

Detailed row columns are assembled left-to-right as enabled:
1. permissions
2. size
3. owner
4. group
5. modified time
6. git status pair (`--git`)
7. repo status marker (`--git`)
8. styled name (and symlink target arrow when `-x`/`--show-targets` is active)

## Practical Examples

```bash
# Fast type-sorted top-level list
./target/release/twig

# Long view equivalent to -Lptos --show-targets
./target/release/twig -l ~/Dev

# Include hidden files and classify entries
./target/release/twig -aF ~

# List-only mode without metadata columns
./target/release/twig -L ~/Dev

# Show symlink targets in compact mode
./target/release/twig -x /home/lewis/.local/bin/twig

# True size with hardlink dedupe (default)
./target/release/twig -S ~/Downloads

# True size without hardlink dedupe
./target/release/twig -S -H ~/Downloads

# Git-aware long listing
./target/release/twig -l --git .

# Show Git root status for child directories (same --git flag)
./target/release/twig -l --git ~/src

# Reverse by date
./target/release/twig --sort time -r

# Emit shell cache files for last shown dirs/files
./target/release/twig --cache-raw ~/Dev
```

## Dependencies

Core crates:
- `clap` for CLI parsing
- `jwalk` for fast traversal
- `rayon` for parallel recursive stats
- `ntfs` for MFT-based NTFS recursive scanning
- `lscolors` + `nu-ansi-term` for styling
- `chrono` for timestamp formatting
- `users` for uid/gid resolution
- `jemallocator` for allocator performance

## Notes and Limits

- Current listing depth is one directory level.
- Git status is computed via NUL-delimited `git status --porcelain=v1 -z` records.
- Repo-root marker is shown only for directories that are Git toplevel roots.
- Size units are decimal text with `K/M/G` suffixes, one decimal above bytes.
- `--cache-raw` is disabled automatically when stdout is not a TTY.
- NTFS MFT fast path generally requires raw block-device access (often root privileges).

## Development

```bash
cargo fmt
cargo build --release
cargo test
```

Unit tests cover CLI aliases and mode separation, Git status translation,
filesystem block rounding/hidden-name handling, and rendering path/size
formatting. Larger output behavior is still checked with fixture directories
and manual command checks.
