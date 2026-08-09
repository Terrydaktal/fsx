# ETA Estimator

The live estimator is implemented by `TransferEtaEstimator` in `src/main.rs`.
It predicts the remaining logical transfer time while accounting for known file
work, recent throughput regimes, and approximate downstream I/O backlog.

## Inputs

The progress ticker samples every 200 ms:

- logical bytes accepted by the transfer loop (`write_all_total`);
- planned logical bytes;
- process I/O counters from `/proc/<pid>/io`;
- device counters from `/proc/diskstats`;
- completed files by size bin and completed directories;
- the pre-scanned remaining file and directory workload.

The estimator aggregates byte observations into approximately one-second
statistical buckets. The 200 ms cadence remains a UI requirement and is not
treated as independent evidence at every tick.

## Regime model

Positive bucket rates are modelled in log space. The estimator maintains up to
64 online run-length hypotheses. Each hypothesis contains:

- the time since its most recent regime change;
- a running log-rate mean and variance;
- a normalized posterior probability.

The predictive likelihood is Student-t-like, which reduces the impact of one
short burst or one delayed sample. A constant-hazard change model provides an
independent regime-change signal. A deterministic guard confirms severe changes
when a sequential byte rate falls below 12.5% of the previous capacity, or rises
above 8 times it. Moderate changes use 60% and 1.67 times with a longer
confirmation period.

The whole-transfer average is not used as a permanent forecast weight. Once a
sequential-capacity change is confirmed, the remaining-byte forecast uses the
new regime instead of retaining the obsolete cache-backed rate.

## Workload model

The scan is represented by eight logarithmic regular-file size bins and a
directory count. Every completed file increments its size-bin count and byte
count atomically. Every completed directory increments the directory count.

The estimator learns fixed object overhead online. For each one-second sample it
subtracts the estimated byte service time from elapsed time, then updates the
observed per-file-bin and per-directory overhead. The remaining producer time
is:

```text
remaining bytes / sequential capacity
  + remaining file overhead
  + remaining directory overhead
```

This prevents a temporary run of tiny files from redefining the large-file
device capacity. Conversely, when the transfer is dominated by tiny files, the
learned object cost prevents the current tiny-file byte rate from being applied
to all remaining large bytes.

The model uses the planned workload histogram. It does not claim an exact
future operation order because the Rust backend executes regular files in
parallel; the manifest's lexical order is not a reliable execution order.

## Pipeline model

When compatible counters are available, the estimator calculates advisory
backlog values:

```text
write backlog = process write_bytes - device write-complete
read backlog  = process read_bytes  - device read-complete
```

The corresponding drain time is estimated from the current device-completion
rate. Producer time and drain time overlap, so the larger value controls the
pipeline estimate rather than the two values being blindly added.

`/proc/diskstats` is device-wide and may include unrelated processes. Therefore
this part is intentionally treated as approximate and lowers confidence rather
than pretending it is a per-process durable-write counter.

## Display stability

The displayed ETA is derived from a predicted finish timestamp rather than
smoothing remaining seconds directly:

- stable operation uses a confidence-dependent two-to-six-second response;
- a confirmed capacity regime change updates immediately;
- a short zero-progress interval advances the predicted finish timestamp so the
  ETA freezes instead of counting down falsely;
- after five seconds without progress the numerical ETA is suppressed as
  `unknown` by the renderer;
- the displayed value is rounded only at the final presentation step.

The estimator internally keeps P10, P50, P90, confidence, and activity state.
Set `COPY_RS_ETA_DEBUG=1` to append those values to the live progress line.
Normal output continues to show the P50 value to preserve the existing layout.

## Completion semantics

The transfer ETA targets logical completion of the copy loop. Flush and cleanup
are separate phases in the command and continue to report their own counters.
The pipeline estimate can account for observed downstream drain before logical
completion, but no userspace counter can prove durable device completion until
the explicit flush operation returns.

## Known limits

- Device-completion counters remain device-wide rather than per-process.
- Extended attributes, sparse extents, hardlinks, and metadata operations are
  not currently separate workload classes.
- Remote and rsync transfers without a local manifest use the robust byte-regime
  model but cannot use file-composition costs.
- The estimator has no persisted cross-command hardware history; it learns from
  the current transfer and its pre-scan.

These limits are surfaced through the confidence value rather than hidden by a
false precision claim.

## Failure case covered

The previous failure displayed a 48-second ETA while approximately 28 GiB
remained at 9.7 MiB/s after a cache-backed phase. The current model detects the
sequential capacity collapse within the confirmation window, starts the rate
estimate at the detected transition, and no longer blends the old whole-transfer
average into the forecast.
