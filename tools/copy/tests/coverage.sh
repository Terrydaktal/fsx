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
cargo +nightly llvm-cov report --branch --summary-only \
	--fail-under-lines 50 --fail-under-branches 35
