# fsx debugging contract

The workspace uses the Stateful profile because `fsxd` is a concurrent daemon
and Copy is a multithreaded, journaled mutation pipeline. Tree, Twig and the
Unearth client use the same build-identity and failure-output substrate, but
do not start resident diagnostic services.

## Build modes

- `release-minimal` is the normal optimized release profile. Optional Copy
  fault hooks are compiled out. Build identity is embedded without runtime
  filesystem or subprocess access.
- `release-observable` is the same optimized configuration with bounded,
  release-safe evidence. fsxd's metrics remain disabled unless requested.
- `runtime-activated` is selected with `--watch-metrics`; it is bounded by a
  TTL and byte limit. `FSX_DIAGNOSTICS=off` disables optional metrics globally.
- `diagnostic` is a separate Cargo profile used for crash, signal and mutation
  tests. It is not a deployment artifact.

## Identity and artifacts

Every executable accepts `--build-info`. The JSON includes package, version,
Git commit and dirty state, target, profile and compiler. The release artifact
script builds `release-symbols`, splits private DWARF by ELF Build ID, strips
the deployable copy and writes a checksum/identity manifest:

```bash
tools/build/release-artifacts.sh
```

Keep the matching `bin/`, `debug/`, `manifest.tsv` and `build-info.txt` files
together. A symbol file without the matching Build ID is not a valid
post-mortem artifact.

For a live daemon that is hung or consuming unexpected CPU, capture first and
only then decide whether to restart it:

```bash
tools/build/capture-fsxd.sh "$FSXD_PID" "$HOME/.cache/fsx/captures/$FSXD_PID"
```

The capture is bounded by a ten-second debugger timeout, stores `/proc` state,
all-thread stacks when GDB is available, and records coredump metadata without
changing the daemon's configuration.

## Live evidence

Use `unearth --watch-status` for human output and
`unearth --watch-status-json` for automation. The JSON command is read-only,
bounded to 256 watcher rows and uses a 250 ms SQLite busy timeout. It labels
the result `best-effort` and reports `available=false` when the database cannot
be read promptly.

Each watcher also exposes stable `error_code`/`error_code_numeric` fields,
which are `null` for a healthy state; human-readable error text is retained
unchanged. The `history` array is a bounded semantic flight recorder of watcher
state transitions (claim, reconcile, recovery, overflow and failures). It
stores at most 256 events per root and reports overwritten records through
`history_dropped`. This is deliberately a transition log, not a raw filesystem
event stream. `tasks` reports the durable state of the watcher, database,
query-server and metrics tasks, while `locks` reports the known index/cache
lock paths and ownership snapshot without waiting on those locks.

Metrics are intentionally activated, not always-on:

```bash
fsxd --watch-metrics "$XDG_CACHE_HOME/fsx/fsxd.tsv" \
  --watch-metrics-ttl 300 --watch-metrics-max-bytes 16777216 "$HOME"
```

The report is capped by both duration and size. It may contain paths in the
selected output location, so protect that directory appropriately.

## Copy recovery evidence

Copy journals are mode 0600, fsynced at transitions, and retain an operation
ID, build commit, transition sequence and timestamps. They cover local and
remote operations. At most 128 journal files are retained. Journals are
recovery evidence, not a multi-user accountability audit trail; if Copy is
deployed as a privileged shared service, an authenticated audit layer is still
required.

## Assurance

Run the normal workspace checks with `--locked`, then the diagnostic-only
fault suite and overhead smoke test:

```bash
cargo test --locked --workspace --all-targets --all-features
cargo build --locked --profile diagnostic -p copy-rs --features diagnostic-hooks
COPY_RS_COPY_BIN="$PWD/target/diagnostic/copy-rs" \
  python3 -m unittest discover -s tools/copy/tests -v
python3 tools/build/diagnostic-overhead.py
```

The overhead script checks that test hooks are absent from the normal release
binary and compares release/diagnostic preview medians. It is a guardrail, not
a proof of mathematical zero overhead; storage durability and activated
metrics have their own declared costs.

To verify symbols on a clean machine, keep the artifact directory isolated from
the build host and run:

```bash
tools/build/release-artifacts.sh
tools/build/verify-symbolization.sh "$(find target/fsx-artifacts -mindepth 1 -maxdepth 1 -type d -print -quit)"
```

The verifier uses only the stripped binary, its matching private DWARF file and
system `addr2line`; it rejects a missing/mismatched Build ID or an unresolved
`??:??` result.

The same artifact carries an exact source snapshot. Retrieve it without a
checkout on the analysis machine with:

```bash
tools/build/retrieve-source.sh ARTIFACT_DIR OUTPUT_DIR
```

`--watch-status-json` also records the effective watcher configuration and its
provenance (`defaults`, `cli+defaults`, or `env+defaults`) in a bounded restart
history. Cache identity includes the index schema, generation, build SHA and
database identity. When `FSX_DIAGNOSTICS_REDACT=1` is set, paths are replaced by
stable hashes and error details are replaced with `redacted`.

The metrics TSV includes observed queue, batch and cache high-water marks. A
stale watcher owner is retained as an `external-exit` history event; Linux
cgroup `oom_kill` counters are exposed in the status snapshot. The systemd
unit enables core dumps and `OOMPolicy=stop`, while an fsxd panic writes a
0600 JSON marker under the fsx cache for post-mortem collection.
