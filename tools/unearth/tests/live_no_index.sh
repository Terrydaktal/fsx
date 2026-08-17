#!/usr/bin/env bash
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
TEST_BASE="$(mktemp -d "${TMPDIR:-/tmp}/unearth-no-index.XXXXXX")"
trap 'rm -rf -- "$TEST_BASE"' EXIT

ROOT="$TEST_BASE/root"
CACHE="$TEST_BASE/cache"
TARGET_DIR="$TEST_BASE/target"
mkdir -p "$ROOT/nested" "$CACHE"
printf 'needle\n' >"$ROOT/nested/needle.txt"

cargo build --quiet --release --manifest-path "$WORKSPACE_ROOT/Cargo.toml" \
	--target-dir "$TARGET_DIR" -p unearth --no-default-features
BIN="$TARGET_DIR/release/unearth"

output=$(XDG_CACHE_HOME="$CACHE" "$BIN" --live -F needle "$ROOT")
if [[ "$output" != *"$ROOT/nested/needle.txt"* ]]; then
	echo "live full-path search did not return the expected path" >&2
	exit 1
fi
if find "$CACHE" -type f -print -quit | grep -q .; then
	echo "live-only search created cache files" >&2
	find "$CACHE" -type f >&2
	exit 1
fi

if "$BIN" --no-index --index needle "$ROOT" >/dev/null 2>&1; then
	echo "conflicting --no-index/--index options were accepted" >&2
	exit 1
fi

echo "Unearth live-only no-index test passed."
