# copy

Rust CLI for local filesystem transfers with preview/confirm flow.

## Requirements

- Linux
- Rust toolchain (`cargo`)
- `rsync` (used for remote endpoints and privileged transfers)

## Project Structure

```text
copy/
├── .gitignore
├── Cargo.toml
├── copy                  # launcher script (builds + runs Rust binary)
├── README.md
├── src/
│   ├── app/              # command dispatch and local/remote lifecycle
│   ├── domain/           # shared transfer, progress, and ETA types
│   ├── output/           # preview, summary, terminal rendering, ETA model
│   ├── plan/             # path resolution, scanning, collision policy
│   ├── transfer/         # Rust I/O, rsync, cleanup, backup, telemetry
│   ├── cli.rs             # argument parsing and help
│   └── runtime.rs         # media detection, worker and writeback limits
└── tests/
    ├── test_cli_matrix.py
    └── test_copy_cli.py
```

## Command

```bash
./copy [OPTIONS] [--preview] [--preview-lite] SOURCE DESTINATION
```

- Default mode: copy
- Move mode: `-m`, `--move`

## Flags

- `-m`, `--move`
  - Move mode (transfer then remove source data).
- `-s`, `--sudo`
  - Run privileged transfer/removal commands through `pkexec`.
- `-o`, `--overwrite`
  - Replace conflicting destination target instead of merge behavior.
- `-c`, `--contents-only`
  - Merge source contents directly into destination path (no source-basename nesting).
- `-b`, `--backup`
  - Create timestamped backup when destination data would be merged/replaced.
- `--sync`
  - Make the destination tree match the source using native Rust transfer and cleanup phases for local, non-elevated operations.
  - Copy files whose type, size, or modification time differs, then delete destination-only entries after the transfer has flushed.
  - Remote and `--sudo` sync operations retain the rsync backend.
- `--verify`
  - Hash copied regular files with SHA-256 after publication and compare source and destination bytes.
  - Local Rust backend only. Verification is opt-in because it performs a second read pass over each regular file.
- `--replace-dest-symlink`
  - Replace the destination link itself; without it, a destination symlink is followed for regular-file copies.
- `--collision POLICY`
  - Select the winner for file-vs-file conflicts inside local directory merges.
  - Copy defaults to `source:metadata-differs`: replace a colliding destination when its type, size, or modification time differs from the source.
  - Move defaults to `source:always`: transfer the explicitly supplied source before source cleanup.
  - `source:size-differs`, `source:newer`, and other conditional policies remain available explicitly.
  - Preview `Mod`/`Ident` classification is policy-independent: regular files are `Ident` only when destination type, size, and modification time match. The selected collision policy separately controls whether they are transferred.
  - Sync uses its own exact-mirror quick check and rejects `--collision`.
- `-v`, `--verbose`, `--showall`
  - Show hierarchical preview: up to 5 changed entries per level (modified first), expand only modified folders, and abbreviate remaining new/modified/unchanged/removed counts.
- `--preview`
  - Run only the preview phase and exit (no confirmation prompt, no transfer).
- `--preview-lite`
  - Faster preview-only mode that skips exact byte scanning when destination tree is brand-new.

## Backend Selection

- Preview is always done in Rust using `jwalk` traversal + `rayon` parallel comparison.
- Local, non-elevated copy, move, and sync operations use the Rust backend.
- The Rust backend tunes worker count, buffering, and writeback pacing for NVMe, HDD, and other media.
- Remote endpoints and `--sudo` force the rsync backend.
- Local operands stay as raw OS paths through argument parsing and resolution, so non-UTF-8
  filenames are preserved. Remote endpoint syntax remains UTF-8 text by definition.
- Rust regular-file and symlink replacements are staged and published atomically; interrupted copies leave only disposable `.copy-rs-partial-*` files.
- Final regular-file creation and atomic publication use descriptor-relative, no-follow parent opens on Linux. A symlinked ancestor is rejected rather than allowing a path race to redirect the transfer; an existing final destination symlink keeps the documented follow-or-replace policy.
- Every local operation, including multi-source batches, has a durable journal under `$XDG_STATE_HOME/copy-rs` (or `$HOME/.local/state/copy-rs`). Journal records are mode 0600 and fsynced through `planned`, `transferring`, `published`, `failed`, and `complete` states. Only journals that stop during an active transfer or publication are reported as interrupted; validated failures are retained for diagnostics without creating a false crash warning. A crash leaves the journal for inspection and the next operation reports it; staging and the idempotent planner make retrying safe without silently resuming an unknown partial transfer.
- The Rust backend handles `SIGINT` and `SIGTERM` at copy-buffer checkpoints, returns a non-zero interrupted status, and skips move cleanup when the transfer was interrupted. Rsync-backed modes also terminate and reap their child process when the wrapper receives either signal.
- Rsync uses `--partial` and `--protect-args`, but exit status 24 is treated as an incomplete transfer and never committed as a move.
- Incomplete source or destination scans fail closed before sync deletion or move cleanup.
- Local move cleanup validates source identity and destination content before deletion, then flushes the source filesystem separately.
- ETA forecasting uses the planned operation order, actual out-of-order completion markers, per-file-size-bin fixed/byte cost models, capacity-regime detection, and a stage-aware writeback forecast.
- ETA P10/P50/P90 values come from sampled model forecasts rather than fixed display offsets. Completed forecasts are persisted as numeric, path-free priors keyed by the source/destination device pair and media class under `$XDG_STATE_HOME/copy-rs/eta-priors.v2` (or `$HOME/.local/state/copy-rs/eta-priors.v2`). Set `COPY_RS_DISABLE_ETA_PRIORS=1` to disable loading and saving them.

## Performance Build Settings

- `copy` builds and runs `../../target/release/copy-rs` by default.
- Release profile uses aggressive optimization (`opt-level=3`, `lto=fat`, `codegen-units=1`, stripped symbols)
  with unwinding enabled so transfer cleanup can return errors rather than aborting the process.
- The workspace release build does not require host-specific `target-cpu=native` tuning, keeping artifacts portable.

## Runtime Behavior

- `SOURCE/*` is treated as contents-only mode (same as `-c` on `SOURCE/`).
- Parent/self-overlap safety is enforced.
- Local mode performs a destination free-space preflight using filesystem stats before transfer (no sudo required).
- Local Rust copy/sync opens every planned regular source before confirmation and aborts without destination writes if any file is unreadable.
- Move mode cleans empty source directories after transferred files are removed.
- Directory and file atime/mtime are preserved; hard-linked regular files are recreated as hard links when the manifest identifies them.
- HDD scheduler changes are disabled by default because they are system-wide and cannot be safely restored after interruption. Set `COPY_RS_SET_HDD_SCHEDULER=1` to opt in.
- Remote moves are refused: remote durability and post-transfer source cleanup cannot be verified safely by the local process.
- Mode line and preview output remain compatible with the previous CLI behavior.

Copy consumes the sibling `fsx` crate for metadata snapshots used by generic tree counts. Transfer
manifests, collision policy, hard-link recreation, sparse/reflink copying, remote execution, and
progress/ETA behavior remain Copy-specific because they are transfer decisions rather than
filesystem facts.

## Build

```bash
cargo build --release
```

The launcher `./copy` auto-builds the workspace binary at `../../target/release/copy-rs` when needed.

## Test

```bash
python3 -m unittest discover -s tools/copy/tests -v
```

This includes real localhost SSH/rsync round trips, process-level signal tests, every hard-crash
journal/publication boundary, corruption checks, and adversarial mutation between preview,
preflight, execution, and publication. `tests/coverage.sh` records workspace LCOV output and makes
CI enforce measured line and branch floors. Tests that genuinely
need authority remain explicit opt-ins: run `COPY_RS_RUN_PKEXEC_TEST=1` in an interactive Polkit
session for the real `pkexec` path, and run the isolated loopback-filesystem harness once through
`pkexec bash -c 'COPY_RS_RUN_ROOT_FAULT_TESTS=1 /absolute/path/to/test_copy_linux_faults.sh'` for
ENOSPC, read-only, and removed-device failures. The harness only creates a temporary image and loop
mount beneath `/tmp` and validates its exact targets before cleanup.
