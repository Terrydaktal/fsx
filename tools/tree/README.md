# tree

A modern, high-performance version of the `tree` utility written in Rust.

## Features

- **Blazing Fast**: Uses `jwalk` for parallel directory traversal.
- **Modern Terminal Support**: Includes **OSC 8 Hyperlinks** (clickable files and directories) and respects the `LS_COLORS` environment variable.
- **Smart Truncation**: Always displays everything at the top level (depth 1), but truncates subdirectories (depth 2+) based on the `-T <value>` parameter with an `... and x more` entry.
- **Type Classification**: Supports the `-F` flag to add suffixes (`/` for directories, `@` for symbolic links, and `*` for executables).
- **Raw Path Cache**: Supports `--cache-raw` to write the currently displayed paths into cache files for downstream shell tooling.
- **Optimized**: Built with `jemalloc` for memory efficiency and compiled with `target-cpu=native` for maximum performance on your hardware.

## Installation

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable or 2024 edition support)

### Building from Source

```bash
git clone https://github.com/Terrydaktal/tree.git
cd tree
cargo build --release
```

The optimized binary will be available at `./target/release/tree`.

### System Integration (Linux/macOS)

```bash
ln -sf $(pwd)/target/release/tree ~/.local/bin/tree
```

## Usage

```bash
tree [OPTIONS] [PATH]
```

### Options

- `-a`: Toggle hidden files visibility (repeat to toggle back; e.g. `-a -a` cancels).
- `-L <MAX_DEPTH>`: Max depth to display (default: 100). A lone numeric positional arg is accepted as shorthand (e.g. `tree 3` or `tree -L 2 3` uses depth `3`). Use `tree -- 3` to force path `3`.
- `-F`: Classify (add `/` for dirs, `@` for symlinks, `*` for executables).
- `-T, --trunc <TRUNC>`: Truncate depth 2+ entries to this value (default: 10).
- `-M, --hide-more-count`: Hide `... and N more` summary rows.
- `-d, --dirs-only`: Show directories only.
- `-G, --no-expand-git`: Toggle `.git/` expansion state (repeat to toggle back; e.g. `-G -G` cancels).
- `--deep`: Alias for `-L 20 -T 2`.
- `-f, --follow-links`: Follow symbolic links.
- `-S, --sizes`: Show proper recursive directory sizes (like `dust`).
- `-H, --no-dedupe-hardlinks`: Disable inode dedup for `--sizes` (faster, may double-count hardlinks).
- `-t, --times`: Show file modification times.
- `-c, --counts`: Show total recursive counts as `dirs` and `files` columns before the tree.
- `-l`: Alias for `-Stc` (show sizes, times, and counts).
- `-r, --reverse`: Reverse the final displayed output lines.
- `--cache-raw`: Write shown full paths to session-scoped files in `/tmp/fzf-history-$USER/`:
  - `universal-last-dirs-<pid>`
  - `universal-last-files-<pid>`
- `--sort <FIELD> <ORDER>`: Sort all levels by `name`, `size`, or `time` in `asc` or `desc` order.
- Default ordering (without `--sort`) is:
  - `-S` only: `size desc`
  - `-t` only: `time desc`
  - `-c` only: `(dirs + files) desc`
  - otherwise: type grouping (directories first), then alphabetical by name
- `-j, --threads <THREADS>`: Number of threads to use (default: 8).
- `--color[=always|auto|never]`: Control ANSI color output (default: `never`; bare `--color` means `always`).
- `--hyperlink[=always|auto|never]`: Control OSC 8 hyperlinks (default: `auto`; bare `--hyperlink` means `always`).
- `-h, --help`: Print help.
- `-V, --version`: Print version.

## Project Structure

- `src/main.rs`: The core logic for directory traversal, tree construction, and rendering.
- `Cargo.toml`: Dependency and optimization profile configuration.
- `.cargo/config.toml`: Architecture-specific optimization flags.

## License

MIT
