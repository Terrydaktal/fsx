# fsx workspace

The `fsx` workspace contains the shared filesystem fact layer and the four
command-line tools built on top of it.

## Project Structure

```text
fsx/
├── Cargo.toml           workspace manifest and shared dependencies
├── crates/
│   └── fsx/              shared library crate
└── tools/
    ├── tree/             hierarchical filesystem renderer
    ├── twig/             fast one-level listing and true-size renderer
    ├── unearth/          recursive filesystem search and index client
    ├── fsxd/             shared live filesystem index daemon
    └── copy/             local/remote copy and move utility
```

The tools retain their own command-line policy, output layout, search behavior,
transfer decisions, and SQLite/watcher policy. The shared crate owns reusable
filesystem facts and mechanisms: direct metadata accessors, optional traversal
primitives, hardlink keys, path handling, Git-ignore/status parsing, terminal
escaping, LS_COLORS matching, NTFS namespace filtering and data sizing, cached
mount lookup with explicit symlink policy, formatting, secure raw path-cache
output, and optional clean-index aggregation.

Twig and Unearth also share the `ls`-style modified/activity timestamp layout
and dim terminal style, so equivalent long-output columns do not drift between
the listing and search tools.

Traversal policy is shared wherever the workload permits it. Twig's ordinary
filesystem aggregation now consumes `fsx::scan::scan_top_level`, including its symlink,
hardlink, and completion/error contract; Unearth's live walker exposes the same
incomplete-result distinction. Tree's hierarchical renderer, Twig's NTFS/MFT
fast path, Unearth's streaming/indexed pipeline, and Copy's one-pass planner
retain specialized traversal where their output or transfer semantics require
it, but they no longer silently invent different ordinary-walk error policies.

The live index owner is the `fsxd` daemon. Its canonical database and socket
live under `$XDG_CACHE_HOME/fsx/index` or `~/.cache/fsx/index`; the old
`unearthd` executable and service remain compatibility entry points during
migration. A legacy `~/.cache/unearth/index/unearth.db` is copied into the
canonical database location when the new daemon first initializes it.

OSC 8 hyperlinks are also shared. `tree`, `twig`, and `unearth` use the same
`file://` URI encoding and PCManFM-compatible parent `?select=` targets. Path
listings split the visible prefix from the basename: the prefix selects the
entry in its parent and the basename opens the entry. Basename-only listings
link directly to the entry, allowing directories to open normally and terminal
middle-click handlers to receive the directory path.

The feature matrix is intentionally narrow:

| Consumer | fsx features | Purpose |
| --- | --- | --- |
| `tree` | `colors`, `git`, `ignore`, `terminal` | Git display, lazy ignore rules, shared LS_COLORS, hyperlinks |
| `twig` | `colors`, `git`, `index`, `ntfs`, `scan`, `terminal` | Indexed-or-live aggregation, NTFS sizing, shared Git/LS_COLORS, hyperlinks |
| `unearth` | `colors`, `ignore`, `ntfs`, `terminal` | LS_COLORS, shared ignore rules, NTFS search, terminal output |
| `copy` | `terminal` | Metadata/mount primitives and safe terminal escaping for previews |

Raw path output is written to `/tmp/fzf-history-$USER/universal-last-dirs-$fish_pid`
and `universal-last-files-$fish_pid` with private directory/file permissions.
`FISH_PID` is accepted as a compatibility fallback when the shell does not
export fish's lowercase variable. Writers serialize per shell PID and skip
newline-containing names rather than corrupting the line-delimited cache.

Copy's local backend additionally uses durable operation journals under
`$XDG_STATE_HOME/copy-rs` (or `$HOME/.local/state/copy-rs`) and descriptor-relative
no-follow destination publication on Linux. `copy --verify` enables an opt-in SHA-256
post-publication verification pass for local regular-file transfers.

SQLite path keys used by Unearth are lossless escaped UTF-8 strings supplied by
`fsx::encode_lossless_path`; indexed binary output decodes them back to original OS bytes.

## Shared Performance Paths

`tree -S -L 1` and `tree -S -L 2` use a specialized ancestor aggregation scan:
they retain only directories that can be rendered and avoid building a full
path graph. General recursive size scans seed each directory inode once and
roll child totals upward without counting directory metadata twice.

Twig first queries the fsx index at
`~/.cache/fsx/index/fsx.db` when a clean live watcher covers the root
and allocated sizes are present. fsxd transactionally maintains compact direct
statistics per indexed directory, so Twig obtains the root and every immediate
child aggregate in one shared query instead of rescanning all indexed entries
or opening one query per child. Candidate-only `(device, inode)` metadata keeps
the indexed result exact when default hardlink deduplication is enabled. Twig
falls back to its live scanner when the index is missing, stale, incomplete,
filtered, or on the NTFS specialist path. Set `FSX_INDEX_DB` to override the
database location.

The shared mount resolver caches `/proc/self/mountinfo`, matches mount-point
boundaries rather than string prefixes, and exposes `Follow` and `Preserve`
final-symlink policies. Copy uses `Preserve` so telemetry does not silently
change the transfer target.

The Unearth index stores logical byte length, allocated block size, direct
directory statistics, and hardlink identity only for entries with multiple
links. Entry triggers keep direct statistics current across fsxd inserts,
updates, moves, and deletes. Existing databases are upgraded in place; old
binary manifest sidecars are invalidated by the manifest version bump and
rebuilt on demand.

## Build

Build or test the complete workspace from this directory:

```bash
cargo build --workspace
cargo test --workspace
cargo build --workspace --features unearth/watcher
```

Individual binaries remain available through their package names, for example
`cargo run -p tree -- -L 2 .` and `cargo run -p twig -- .`.

## Repository History

The four tool directories currently retain their nested Git repositories and
existing history. The workspace root provides Cargo coordination, but this is
not yet a consolidated Git history: import the nested histories or convert them
to explicit submodules before treating the root as the final publishable
monorepo. Existing installation symlinks must also target this workspace after
building release binaries. The root workspace owns dependency resolution and
CI; nested lockfiles remain only for standalone compatibility.

## Validation

```bash
cargo test --workspace --all-targets
cargo bench -p fsx --bench fsx_primitives
```

The benchmark target measures shared path normalization, formatting, metadata,
and LS_COLORS parsing. Filesystem walker claims should be compared with
representative Tree, Twig, Unearth and Copy workloads before changing their
specialized traversal policies.
