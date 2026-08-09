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
- `--replace-dest-symlink`
  - Replace the destination link itself; without it, a destination symlink is followed for regular-file copies.
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
- Rust regular-file and symlink replacements are staged and published atomically; interrupted copies leave only disposable `.copy-rs-partial-*` files.
- Rsync uses `--partial` and `--protect-args`, but exit status 24 is treated as an incomplete transfer and never committed as a move.
- Incomplete source or destination scans fail closed before sync deletion or move cleanup.
- Local move cleanup validates source identity and destination content before deletion, then flushes the source filesystem separately.
- ETA forecasting uses the planned operation order, actual out-of-order completion markers, per-file-size-bin fixed/byte cost models, capacity-regime detection, and a stage-aware writeback forecast.
- ETA P10/P50/P90 values come from sampled model forecasts rather than fixed display offsets. Completed forecasts are persisted as numeric, path-free priors keyed by the source/destination device pair and media class under `$XDG_STATE_HOME/copy-rs/eta-priors.v2` (or `$HOME/.local/state/copy-rs/eta-priors.v2`). Set `COPY_RS_DISABLE_ETA_PRIORS=1` to disable loading and saving them.

## Performance Build Settings

- `copy` builds and runs `target/release/copy-rs` by default.
- Release profile uses aggressive optimization (`opt-level=3`, `lto=fat`, `codegen-units=1`, `panic=abort`, stripped symbols).
- Host tuning is enabled with `-C target-cpu=native` via `.cargo/config.toml`.

## Runtime Behavior

- `SOURCE/*` is treated as contents-only mode (same as `-c` on `SOURCE/`).
- Parent/self-overlap safety is enforced.
- Local mode performs a destination free-space preflight using filesystem stats before transfer (no sudo required).
- Move mode cleans empty source directories after transferred files are removed.
- Directory and file atime/mtime are preserved; hard-linked regular files are recreated as hard links when the manifest identifies them.
- HDD scheduler changes are disabled by default because they are system-wide and cannot be safely restored after interruption. Set `COPY_RS_SET_HDD_SCHEDULER=1` to opt in.
- Remote moves are refused: remote durability and post-transfer source cleanup cannot be verified safely by the local process.
- Mode line and preview output remain compatible with the previous CLI behavior.

## Build

```bash
cargo build --release
```

The launcher `./copy` auto-builds `target/release/copy-rs` when needed.

## Test

```bash
python3 -m unittest discover -s tests -v
```
