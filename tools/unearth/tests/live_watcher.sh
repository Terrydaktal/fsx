#!/usr/bin/env bash
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
CLIENT_BIN="${UNEARTH_BIN:-${WORKSPACE_ROOT}/target/debug/unearth}"
DAEMON_BIN="${FSXD_BIN:-${UNEARTHD_BIN:-${WORKSPACE_ROOT}/target/debug/fsxd}}"
POLL_ATTEMPTS="${UNEARTH_LIVE_POLL_ATTEMPTS:-400}"
STRESS_COUNT="${UNEARTH_LIVE_STRESS_COUNT:-1000}"
FLOOD_COUNT="${UNEARTH_LIVE_OVERFLOW_COUNT:-0}"
CANCEL_COUNT="${UNEARTH_LIVE_CANCEL_COUNT:-0}"
QUEUE_LIMIT="$(cat /proc/sys/fs/inotify/max_queued_events 2>/dev/null || printf '16384')"
if [[ ! "$QUEUE_LIMIT" =~ ^[0-9]+$ ]]; then
	QUEUE_LIMIT=16384
fi

if [[ ! "$POLL_ATTEMPTS" =~ ^[0-9]+$ || ! "$STRESS_COUNT" =~ ^[0-9]+$ || ! "$FLOOD_COUNT" =~ ^[0-9]+$ || ! "$CANCEL_COUNT" =~ ^[0-9]+$ ]]; then
	echo "UNEARTH_LIVE_POLL_ATTEMPTS, UNEARTH_LIVE_STRESS_COUNT, UNEARTH_LIVE_OVERFLOW_COUNT, and UNEARTH_LIVE_CANCEL_COUNT must be non-negative integers" >&2
	exit 2
fi

if [[ ! -x "$CLIENT_BIN" || ! -x "$DAEMON_BIN" ]]; then
	cargo build --quiet --manifest-path "${WORKSPACE_ROOT}/Cargo.toml" --bin unearth --bin fsxd
fi

TEST_BASE="$(mktemp -d "${TMPDIR:-/tmp}/unearth-live-test.XXXXXX")"
TEST_ROOT="${TEST_BASE}/root"
CACHE_ROOT="${TEST_BASE}/cache"
LOG_FILE="${TEST_BASE}/watch.log"
METRICS_FILE="${TEST_BASE}/watch-metrics.tsv"
RUN_ID="${RANDOM}_$$"
FLOOD_TREE="${TEST_ROOT}/event-flood-${RUN_ID}"
DAEMON_PID=""
CANCEL_PID=""

cleanup() {
	if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
		kill -CONT "$DAEMON_PID" 2>/dev/null || true
		kill -TERM "$DAEMON_PID" 2>/dev/null || true
		for _ in {1..100}; do
			if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
				break
			fi
			sleep 0.02
		done
		kill -KILL "$DAEMON_PID" 2>/dev/null || true
		wait "$DAEMON_PID" 2>/dev/null || true
	fi
	if [[ -n "$CANCEL_PID" ]] && kill -0 "$CANCEL_PID" 2>/dev/null; then
		kill -TERM "$CANCEL_PID" 2>/dev/null || true
		kill -KILL "$CANCEL_PID" 2>/dev/null || true
		wait "$CANCEL_PID" 2>/dev/null || true
	fi
	/usr/bin/rm -rf -- "$TEST_BASE"
}
trap cleanup EXIT

mkdir -p "$TEST_ROOT"
if ((FLOOD_COUNT > 0)); then
	mkdir "$FLOOD_TREE"
fi

if ((CANCEL_COUNT > 0)); then
	cancel_root="${TEST_BASE}/cancel-root"
	cancel_cache="${TEST_BASE}/cancel-cache"
	cancel_log="${TEST_BASE}/cancel.log"
	mkdir "$cancel_root"
	for ((i = 1; i <= CANCEL_COUNT; i++)); do
		: >"${cancel_root}/file-${i}"
	done
	XDG_CACHE_HOME="$cancel_cache" "$DAEMON_BIN" --threads 1 "$cancel_root" >"$cancel_log" 2>&1 &
	CANCEL_PID=$!
	cancel_status=""
	for _ in $(seq 1 "$POLL_ATTEMPTS"); do
		cancel_status="$(XDG_CACHE_HOME="$cancel_cache" "$CLIENT_BIN" --watch-status 2>/dev/null || true)"
		if printf '%s\n' "$cancel_status" | grep -Fq "$cancel_root" &&
			printf '%s\n' "$cancel_status" | grep -Fq $'\tstarting\t'; then
			break
		fi
		if ! kill -0 "$CANCEL_PID" 2>/dev/null; then
			echo "cancellation test watcher exited before startup" >&2
			cat "$cancel_log" >&2
			exit 1
		fi
		sleep 0.02
	done
	cancel_pid="$CANCEL_PID"
	kill -TERM "$cancel_pid" 2>/dev/null || true
	wait "$cancel_pid" 2>/dev/null || true
	CANCEL_PID=""
	if kill -0 "$cancel_pid" 2>/dev/null; then
		echo "startup cancellation test watcher did not stop" >&2
		exit 1
	fi
fi

export XDG_CACHE_HOME="$CACHE_ROOT"

"$DAEMON_BIN" --threads 1 --watch-metrics "$METRICS_FILE" "$TEST_ROOT" >"$LOG_FILE" 2>&1 &
DAEMON_PID=$!

watch_status=""
for _ in $(seq 1 "$POLL_ATTEMPTS"); do
	if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
		echo "live watcher exited during startup" >&2
		cat "$LOG_FILE" >&2
		exit 1
	fi
	watch_status="$("$CLIENT_BIN" --watch-status 2>/dev/null || true)"
	if printf '%s\n' "$watch_status" | grep -Fq "$TEST_ROOT" &&
		printf '%s\n' "$watch_status" | grep -Fq $'\trunning\t'; then
		break
	fi
	sleep 0.05
done

status_line="$(printf '%s\n' "$watch_status" | grep -F "$TEST_ROOT" | head -n 1 || true)"
if [[ -z "$status_line" ]] || ! printf '%s\n' "$status_line" | grep -Fq $'\trunning\t'; then
	echo "live watcher did not become ready" >&2
	cat "$LOG_FILE" >&2
	printf '%s\n' "$watch_status" >&2
	exit 1
fi
backend="$(printf '%s\n' "$status_line" | awk -F '\t' '{print $2}')"
if [[ "$backend" != "fanotify" && "$backend" != "inotify" ]]; then
	echo "unexpected watcher backend: $backend" >&2
	exit 1
fi

query() {
	"$CLIENT_BIN" --index --file --color=never "$1" "$TEST_ROOT" 2>/dev/null || true
}

wait_present() {
	local term="$1"
	local needle="$2"
	local output=""
	for _ in $(seq 1 "$POLL_ATTEMPTS"); do
		output="$(query "$term")"
		if [[ "$output" == *"$needle"* ]]; then
			return 0
		fi
		sleep 0.05
	done
	echo "timed out waiting for indexed path: $needle" >&2
	printf '%s\n' "$output" >&2
	cat "$LOG_FILE" >&2
	exit 1
}

wait_absent() {
	local term="$1"
	local needle="$2"
	local output=""
	for _ in $(seq 1 "$POLL_ATTEMPTS"); do
		output="$(query "$term")"
		if [[ "$output" != *"$needle"* ]]; then
			return 0
		fi
		sleep 0.05
	done
	echo "timed out waiting for indexed path removal: $needle" >&2
	printf '%s\n' "$output" >&2
	cat "$LOG_FILE" >&2
	exit 1
}

initial_file="${TEST_ROOT}/initial-${RUN_ID}.txt"
touch "$initial_file"
wait_present "initial-${RUN_ID}.txt" "$initial_file"

tree="${TEST_ROOT}/live-tree-${RUN_ID}"
mkdir "$tree"
for ((i = 1; i <= STRESS_COUNT; i++)); do
	printf 'live watcher test %s\n' "$i" >"${tree}/file-${i}.txt"
done
final_file="${tree}/file-${STRESS_COUNT}.txt"
wait_present "file-${STRESS_COUNT}.txt" "$final_file"

moved_tree="${TEST_ROOT}/moved-tree-${RUN_ID}"
mv "$tree" "$moved_tree"
moved_final="${moved_tree}/file-${STRESS_COUNT}.txt"
wait_present "file-${STRESS_COUNT}.txt" "$moved_final"
wait_absent "file-${STRESS_COUNT}.txt" "$final_file"

/usr/bin/rm -rf -- "$moved_tree"
wait_absent "file-${STRESS_COUNT}.txt" "$moved_final"

if ((FLOOD_COUNT > 0)); then
	# Stop every watcher thread while flooding a pre-watched directory. When the
	# flood exceeds the kernel queue, this deterministically exercises overflow
	# recovery instead of merely measuring normal event throughput.
	kill -STOP "$DAEMON_PID"
	for ((i = 1; i <= FLOOD_COUNT; i++)); do
		printf 'event flood %s\n' "$i" >"${FLOOD_TREE}/file-${i}.txt"
	done
	kill -CONT "$DAEMON_PID"
	wait_present "file-${FLOOD_COUNT}.txt" "${FLOOD_TREE}/file-${FLOOD_COUNT}.txt"
	if [[ "$backend" == "inotify" ]] && ((FLOOD_COUNT > QUEUE_LIMIT)); then
		overflow_seen=0
		for _ in $(seq 1 "$POLL_ATTEMPTS"); do
			if awk -F '\t' 'NR > 1 && $18 + 0 > 0 { found = 1 } END { exit found ? 0 : 1 }' "$METRICS_FILE"; then
				overflow_seen=1
				break
			fi
			sleep 0.05
		done
		if ((overflow_seen == 0)); then
			echo "event flood exceeded inotify queue but no overflow recovery was recorded" >&2
			cat "$LOG_FILE" >&2
			exit 1
		fi
	fi
fi

if [[ ! -s "$METRICS_FILE" ]] || ! grep -Fq $'timestamp_ms\telapsed_ms\tpid' "$METRICS_FILE"; then
	echo "live watcher metrics file was not written" >&2
	cat "$LOG_FILE" >&2
	exit 1
fi

printf 'live watcher integration passed: backend=%s stress_count=%s overflow_flood=%s cancellation_count=%s\n' \
	"$backend" "$STRESS_COUNT" "$FLOOD_COUNT" "$CANCEL_COUNT"
