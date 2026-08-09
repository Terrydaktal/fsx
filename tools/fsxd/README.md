# fsxd

`fsxd` is the long-running shared filesystem index daemon used by the fsx
tools. It owns the live watcher, the pooled SQLite database, and the Unix
query socket consumed by Unearth and Twig.

Build and run it from the workspace root:

```bash
cargo build --release -p fsxd
./target/release/fsxd "$HOME"
```

The canonical state lives under `$XDG_CACHE_HOME/fsx/index` or
`~/.cache/fsx/index`. The `unearthd` binary remains a compatibility entry point
for existing launchers.
