# Performance implementation plan

This tracks the sixteen findings in the September 2026 performance audit.
Existing unrelated working-tree changes are preserved. No content cache,
search exclusions, weaker freshness checks, or weaker transfer durability are
part of this work. Each step includes regression coverage; end-to-end CPU,
elapsed-time and peak-RSS workloads are added alongside the implementations.

1. [x] Copy: reuse worker buffers for atomic transfers and allocate lazily.
2. [x] fsxd: avoid unchanged directory-stat trigger work; use net deltas.
3. [x] Twig: scope hardlink queries to the requested directory subtree.
4. [x] Recursive sizes: aggregate overlapping requests together, including the index path.
5. [x] Unearth: select limited results before display-only size work, avoid name-sort metadata reads, and cache numeric sort keys.
6. [x] fsxd: compact refresh records and bound reconciliation snapshot memory with safe recovery.
7. [x] Copy: share planning path storage and eliminate redundant planning collections.
8. [x] Tree: compact recursive aggregation state and select children before sorting truncated output.
9. [x] fsxd: replace quadratic reconciliation ancestry searches.
10. [x] Copy: compare content incrementally with early mismatch detection and correct short-read handling.
11. [x] Twig: drain Git subprocess pipes while enforcing a deadline without 10 ms polling.
12. [x] Shared scanning: shard hardlink identities and avoid repeated child-name allocation.
13. [x] Twig: reuse top-level scan metadata for rendering.
14. [x] Unearth: stream eligible styled output with bounded rendering memory.
15. [x] Hyperlinks: retain only bounded reusable parent encodings, not one-use leaf paths.
16. [x] fsxd: reuse scan and metadata worker pools.

## Implementation details and regression evidence

| Step | Implementation | Main regression evidence |
| --- | --- | --- |
| 1 | Task-local transfer buffers are reused by atomic and non-atomic paths; kernel-copy successes allocate no data buffer. | Empty atomic copy allocates nothing; buffer capacity/pointer reuse; sparse, cancellation and publication tests. |
| 2 | Unchanged metadata does not update `dir_stats`; real changes apply one net delta, with a separate cross-directory trigger. Migration retains the old trigger name to avoid duplicate triggers from older writers. | SQL write counter: zero no-op updates, one real same-directory update; NULL/kind/move transitions and repeated migration. |
| 3 | Narrow hardlink queries start from the requested subtree. Whole-root queries use the candidate index; a bounded probe selects that strategy for broad, sparse-hardlink subtrees too. | Query-plan assertions, adaptive strategy checks and exact global/per-child hardlink totals. |
| 4 | An invocation-local path forest folds overlapping live/indexed requests once. Twig groups index rows by immediate child in SQLite. | Overlapping and duplicate requests, hidden/target contents, symlink cycles, missing metadata and prefix siblings. |
| 5 | Name/date top-k selection precedes display-only recursive sizing; numeric sort keys are calculated once. Metadata needed for classifiers is still collected. | Population callback sees only selected name/date rows; size ranking sees all rows; FIFO classifiers and output ordering. |
| 6 | Refresh records keep one reversible path representation. Reconciliation snapshots have a 32 MiB capture cap and 64 MiB queued-payload budget. | Reference-counted budget release, retained reconciliation on overflow, watch coverage after capture discard and successful full rescan. |
| 7 | Destination display paths are generated only when needed. Directory sets borrow paths, file sets share identities, and scan records move into manifests. | Copy integration and preview matrix, including non-UTF-8/percent-escape collision tests. |
| 8 | One combined totals record is retained per visible directory; deeper callbacks feed the last visible ancestor. Parent IDs fold the totals. Only the displayed child prefix is sorted. | All metrics and overflow, selection/omitted totals, deep hardlinks, ignored/unreadable directories and output contracts. |
| 9 | Reconciliation roots and invalidated snapshots are found through ancestor lookups rather than all-pairs prefix searches. | 4,096 disjoint roots plus nested target paths, sibling-prefix separation and both sides of a rename. |
| 10 | Content-policy comparisons read bounded chunks and stop at the first mismatch. Post-transfer SHA-256 verification is unchanged. | Uneven short reads, interrupted reads, length/content mismatches, early exit and propagated I/O errors. |
| 11 | Nonblocking stdout/stderr draining uses `poll` and a child-exit wakeup. The child remains unreaped until timeout handling is complete. | Pipe-sized stdout/stderr, exit status and deadline after pipes close. |
| 12 | Candidate identities use 32 shards. Per-child identity sets are lazy and retrieved once per directory containing candidates. | Concurrent insertion counts, exact scan totals across thread counts and per-scope hardlink tests. |
| 13 | Live and indexed listing paths pass immediate-child metadata to Twig's renderer instead of repeating root enumeration and child stats. | Bounded metadata retention and daemon/live listing parity before and after nested file creation. |
| 14 | Eligible live and indexed results are rendered incrementally, including colours, classifiers, highlights and hyperlinks. | Streamed/buffered byte parity, limits, raw-path cache completeness and indexed styling parity. |
| 15 | One-use leaf encodings are no longer retained. Reusable parent encodings are bounded to 256 entries. | 10,000 unique links stay bounded; direct, selection and split-link bytes remain unchanged. |
| 16 | fsxd reuses its scan/metadata pool for the configured thread count; nested walks use the serial path to avoid pool starvation. | Shared pool identity, serial configuration, nested invocation and live watcher stress test. |

These are bounded optimizations, not claims of constant memory for every mode.
The full initial index refresh still retains its compact record set. Indexed
aggregates still visit subtree directory rows inside SQLite, rather than storing
a new persistent recursive-total cache. Sorting/count-summary modes still need
whole-result information. Explicit raw-path cache mode retains its pre-existing
exact-deduplication sets; streaming eliminates the additional full result and
rendered-string collections without truncating that cache's input.

## Verification

- Targeted unit tests for each changed algorithm and its boundary conditions.
- Workspace tests and output-contract tests for all four tools.
- Copy integration tests, including cancellation, atomic publication and previews.
- Standalone/no-default-feature builds, keeping Android's SQLite-free path viable.
- Formatting and lint checks; no diagnostic/safety feature is disabled for speed.
- Repeatable performance workloads with correctness assertions and CPU/RSS output.

## Results

Verified locally on 2026-09-12:

- 186 Rust tests and five primitive benchmark smoke cases passed with all workspace features/targets.
- 68 output-contract tests passed, including the indexed/live parity and styled/raw-cache regressions added here.
- Copy integration/fault suite: 193 passed, one skipped (explicit opt-in actual sudo invocation).
- The private inotify watcher stress test passed with 1,000 files. No installed daemon was restarted.
- CI's strict Clippy command, formatting and `git diff --check` passed.
- `--no-default-features` checks passed for Tree, Twig, Unearth and Copy. An Android target/device was not available locally; Android cross-checks remain in CI.
- Optimized normal-feature binaries were built under `target/release-symbols/`, without replacing installed release binaries.

The existing insufficient-space integration test also had a timing race: it
requested only one byte more than the current free space. It now uses more than
total filesystem capacity, so unrelated file deletion cannot invalidate the
test's premise. The fixture remains sparse and does not fill the disk.

### Repeating the resource measurements

```sh
cargo build --locked --workspace --profile release-symbols
python3 tools/build/performance-regressions.py --bin-dir target/release-symbols --files 4096 --repeat 5
```

The harness uses a fresh private fixture, asserts search/copy correctness, and
reports median elapsed time, user/kernel CPU and peak RSS. Its small native
measurement parent avoids counting Python's pre-exec heap as the tool's peak
RSS; native monotonic timing avoids Python wait-polling quantization. Fixture
construction, binary hashing and output verification are outside measurements.
CI runs a smaller version and uploads the JSON resource report. Timing is
reported, not used as a flaky pass/fail gate.

The final local run used 4,096 files and five repetitions, after this task's
builds and test runs had finished. Medians were:

| Workload | Elapsed ms | User CPU ms | Kernel CPU ms | Peak RSS KiB |
| --- | ---: | ---: | ---: | ---: |
| Twig live sizes | 5.367 | 4.061 | 12.576 | 72,132 |
| Tree truncated sizes/counts | 8.107 | 6.108 | 14.086 | 81,136 |
| Unearth styled files | 14.386 | 7.391 | 11.725 | 49,384 |
| Unearth overlapping directory sizes | 5.440 | 3.899 | 11.180 | 49,232 |
| Unearth name top-k with sizes | 3.999 | 1.270 | 6.982 | 49,336 |
| Copy preview | 10.803 | 6.960 | 10.020 | 46,572 |
| Copy tiny-file atomic transfers | 58.394 | 32.032 | 216.430 | 51,116 |

CPU time is summed across threads and can exceed elapsed time. Each column
reports its own median; the CPU columns need not correspond to the same run
as the elapsed-time median. These results provide a repeatable reference for
future comparisons, not evidence of a measured improvement over the old code.

This is warm-cache synthetic validation, not a before/after measurement or a
prediction of `/`, `/trash`, cold storage, Android or daemon-wide resource use.
A checked step denotes implemented and tested behavior, not a promise of a
particular speedup on every filesystem.
