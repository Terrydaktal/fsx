# fsx

`fsx` is the shared filesystem fact layer for the Tree, Twig, Unearth, and Copy
tools. It deliberately does not own command-line flags, sorting policy, search
semantics, transfer decisions, or terminal layout.

The crate provides:

- stable metadata snapshots with explicit logical and allocated-size semantics;
- lexical path normalization and missing-leaf path resolution;
- bounded or recursive filesystem scanning with optional aggregation;
- candidate-only hard-link accounting and symlink-cycle protection;
- batched clean-index recursive statistics for a root and its visible children;
- Git-ignore matching and porcelain status parsing;
- terminal escaping, file URI encoding, and compact size/count formatting.
- shared `ls`-style timestamp formatting and ANSI dim detail-column styling.
- reversible Unix path encoding for UTF-8/SQLite boundaries, plus checked saturating arithmetic
  for filesystem aggregate overflow diagnostics.

The first release is Linux-oriented because the consumers use Unix inode,
allocation-block, and mode information. APIs expose optional fields where the
underlying platform cannot provide those values.

## Project Structure

```text
fsx/
├── Cargo.toml
├── README.md
└── src/
    ├── lib.rs          public API and integration tests
    ├── entry.rs        shared path/depth/metadata entry snapshots
    ├── metadata.rs     logical/allocated metadata snapshots and inode keys
    ├── index.rs        freshness and batched exact recursive index aggregates
    ├── mount.rs        Linux mount topology lookup
    ├── path.rs         lexical and missing-leaf path operations
    ├── scan.rs         bounded traversal and recursive aggregation
    ├── ignore.rs       Git-ignore matching
    ├── git.rs          Git porcelain status parsing
    ├── terminal.rs     escaping, file URIs, and OSC 8 links
    ├── format.rs       size and count formatting
    ├── path_cache.rs   fzf history directory/file cache output
    └── error.rs        shared error types
```

The four consumers provide the input flags and choose policies, call the fsx
fact APIs, then render or act on the returned data. `fsx` has no executable or
background pipeline of its own and creates cache files only when a consumer
explicitly calls `path_cache::write_raw_paths`.

`encode_lossless_path` and `decode_lossless_path` are intended for text-only storage boundaries
such as SQLite. They preserve valid Unicode, escape `%`, and encode invalid Unix bytes as `%XX`.
Use the decoded `PathBuf` for filesystem calls and the encoded string for SQL keys. Aggregate
callers can use `overflow::checked_add_u64` to saturate safely while retaining an explicit
overflow signal.
