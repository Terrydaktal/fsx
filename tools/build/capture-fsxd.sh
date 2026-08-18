#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
	echo "usage: $0 PID OUTPUT_DIR" >&2
	exit 2
fi

pid="$1"
output="$2"
if [[ ! "$pid" =~ ^[0-9]+$ || "$pid" == 0 ]]; then
	echo "PID must be a positive decimal process ID" >&2
	exit 2
fi
if [[ ! -r "/proc/$pid/status" ]]; then
	echo "cannot read /proc/$pid; the process may have exited" >&2
	exit 1
fi

mkdir -p -- "$output"
chmod 700 -- "$output"
cp --preserve=mode,timestamps "/proc/$pid/status" "$output/proc-status"
cp --preserve=mode,timestamps "/proc/$pid/limits" "$output/proc-limits" 2>/dev/null || true
cp --preserve=mode,timestamps "/proc/$pid/stack" "$output/proc-stack" 2>/dev/null || true
if [[ -r "/proc/$pid/cmdline" ]]; then
	tr '\0' ' ' <"/proc/$pid/cmdline" >"$output/cmdline"
	printf '\n' >>"$output/cmdline"
fi

if command -v gdb >/dev/null 2>&1; then
	if ! timeout --kill-after=2s 10s gdb -q -batch -nx \
		-ex 'set pagination off' -ex 'thread apply all bt full' -p "$pid" \
		>"$output/gdb-threads.txt" 2>&1; then
		echo "gdb capture did not complete; see gdb-threads.txt" >&2
	fi
else
	echo "gdb unavailable" >"$output/gdb-threads.txt"
fi

if command -v coredumpctl >/dev/null 2>&1; then
	coredumpctl info "$pid" >"$output/coredumpctl-info.txt" 2>&1 || true
else
	echo "coredumpctl unavailable" >"$output/coredumpctl-info.txt"
fi

printf 'captured_pid=%s\ncaptured_at=%s\n' "$pid" "$(date --iso-8601=seconds)" >"$output/README"
printf 'capture directory: %s\n' "$output"
