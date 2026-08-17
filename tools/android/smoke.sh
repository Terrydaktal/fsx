#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
FRIZ_DIR=${FRIZ_DIR:-"$ROOT_DIR/../friz"}
FRIZ_TARGET_DIR=${FRIZ_CARGO_TARGET_DIR:-"$FRIZ_DIR/target"}
SERIAL=${ANDROID_SERIAL:-}
ROOT_CMD=${FSX_ANDROID_ROOT_CMD:-sudo}
TARGET=${ANDROID_TARGET:-aarch64-linux-android}
LOCAL_TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT_DIR/target"}/$TARGET/release
REMOTE_BASE=${FSX_ANDROID_REMOTE_BASE:-/data/local/tmp/fsx-smoke}
REMOTE_BIN="$REMOTE_BASE/bin"
REMOTE_DATA="$REMOTE_BASE/data"

command -v adb >/dev/null 2>&1 || {
	echo "adb is required" >&2
	exit 1
}
[[ -x "$LOCAL_TARGET_DIR/tree" ]] || {
	echo "build Android binaries first" >&2
	exit 1
}
[[ -x "$LOCAL_TARGET_DIR/unearth" ]] || {
	echo "missing unearth binary" >&2
	exit 1
}
[[ -x "$LOCAL_TARGET_DIR/twig" ]] || {
	echo "missing twig binary" >&2
	exit 1
}
[[ -x "$LOCAL_TARGET_DIR/copy-rs" ]] || {
	echo "missing copy-rs binary" >&2
	exit 1
}
[[ -x "$FRIZ_TARGET_DIR/$TARGET/release/friz" ]] || {
	echo "missing friz binary" >&2
	exit 1
}

adb_args=()
[[ -n "$SERIAL" ]] && adb_args+=(-s "$SERIAL")
adb_cmd=(adb "${adb_args[@]}")

cleanup() {
	"${adb_cmd[@]}" shell "rm -rf '$REMOTE_BASE'" >/dev/null 2>&1 || true
}
trap cleanup EXIT

"${adb_cmd[@]}" shell "rm -rf '$REMOTE_BASE'; mkdir -p '$REMOTE_BIN' '$REMOTE_DATA/src' '$REMOTE_DATA/dst'"
for binary in tree unearth twig copy-rs; do
	"${adb_cmd[@]}" push "$LOCAL_TARGET_DIR/$binary" "$REMOTE_BIN/$binary" >/dev/null
done
"${adb_cmd[@]}" push "$FRIZ_TARGET_DIR/$TARGET/release/friz" "$REMOTE_BIN/friz" >/dev/null
"${adb_cmd[@]}" shell "chmod 755 '$REMOTE_BIN'/*; printf 'needle\n' > '$REMOTE_DATA/src/needle.txt'"

run_rooted() {
	local command=$1
	"${adb_cmd[@]}" shell "${ROOT_CMD} sh -c \"$command\""
}

run_rooted "$REMOTE_BIN/tree '$REMOTE_DATA/src'"
run_rooted "HOME='$REMOTE_BASE/home' XDG_CACHE_HOME='$REMOTE_BASE/cache' '$REMOTE_BIN/unearth' --live -F needle '$REMOTE_DATA'"
run_rooted "HOME='$REMOTE_BASE/home' XDG_CACHE_HOME='$REMOTE_BASE/cache' '$REMOTE_BIN/twig' '$REMOTE_DATA'"
printf 'y\n' | "${adb_cmd[@]}" shell "${ROOT_CMD} '$REMOTE_BIN/copy-rs' '$REMOTE_DATA/src' '$REMOTE_DATA/dst'"
run_rooted "test -f '$REMOTE_DATA/dst/src/needle.txt'"
run_rooted "test ! -e '$REMOTE_BASE/cache/fsx/index/fsx.db'"
run_rooted "test ! -e '$REMOTE_BASE/cache/fsx/index/fsx.db-wal'"

echo "Rooted Android smoke tests passed."
