#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
cd -- "$repo_root"

cargo +nightly llvm-cov clean --workspace
cargo +nightly llvm-cov --workspace --all-features --all-targets --branch --no-report

# Instrument the binaries used by the process-level Python suites and keep
# their raw profiles in the same directory as the Rust unit-test profiles.
eval "$(cargo +nightly llvm-cov show-env --sh --branch)"
cargo +nightly build --workspace --all-features

COPY_RS_COPY_BIN="$repo_root/target/debug/copy-rs" \
	python3 -m unittest discover -s tools/copy/tests -v
F_BIN="$repo_root/target/debug/unearth" bash tools/unearth/tests/help_matrix.sh
UNEARTH_BIN="$repo_root/target/debug/unearth" FSXD_BIN="$repo_root/target/debug/fsxd" \
	bash tools/unearth/tests/live_watcher.sh

cargo +nightly llvm-cov report --branch --lcov --output-path target/coverage.lcov

# cargo-llvm-cov exposes a line threshold flag, but not a branch threshold
# flag. Keep both thresholds enforceable by reading the stable JSON summary;
# this also leaves the exact percentages in the CI log for later diagnosis.
summary_json=$(mktemp "${TMPDIR:-/tmp}/fsx-coverage.XXXXXX.json")
trap 'rm -f -- "$summary_json"' EXIT
cargo +nightly llvm-cov report --branch --summary-only --json >"$summary_json"
jq -e '
  (.data[0].totals.lines.percent >= 25)
  and (.data[0].totals.branches.percent >= 15)
' "$summary_json" >/dev/null
jq -r '"coverage: lines \(.data[0].totals.lines.percent|round)% branches \(.data[0].totals.branches.percent|round)%"' "$summary_json"
