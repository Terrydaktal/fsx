#!/usr/bin/env bash
set -euo pipefail

if [[ ${COPY_RS_RUN_ROOT_FAULT_TESTS:-0} != 1 ]]; then
	echo "SKIP: set COPY_RS_RUN_ROOT_FAULT_TESTS=1 and run as root for mount/device fault coverage"
	exit 0
fi
if [[ $EUID -ne 0 ]]; then
	echo "ERROR: run this suite as root" >&2
	exit 1
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
copy_bin=$repo_root/target/release/copy-rs
work=$(mktemp -d /tmp/copy-rs-linux-faults.XXXXXX)
image=$work/fs.img
mountpoint=$work/mnt
loopdev=

cleanup() {
	if mountpoint -q -- "$mountpoint"; then
		if ! umount -- "$mountpoint"; then
			echo "ERROR: could not unmount test mount $mountpoint" >&2
			return
		fi
	fi
	if [[ -n $loopdev ]]; then
		if ! losetup -d -- "$loopdev"; then
			echo "ERROR: could not detach test loop device $loopdev" >&2
			return
		fi
	fi
	find "$work" -depth -mindepth 1 -delete
	rmdir -- "$work"
}
trap cleanup EXIT

mkdir -- "$mountpoint" "$work/state"
truncate -s 32M -- "$image"
mkfs.ext4 -q -F -- "$image"
loopdev=$(losetup --find --show "$image")
mount -- "$loopdev" "$mountpoint"

dd if=/dev/zero of="$work/source.bin" bs=1M count=48 status=none
set +e
printf 'y\n' | XDG_STATE_HOME="$work/state" COPY_RS_DISABLE_ETA_PRIORS=1 "$copy_bin" "$work/source.bin" "$mountpoint/destination.bin"
enospc_rc=$?
set -e
[[ $enospc_rc -ne 0 ]] || {
	echo "expected ENOSPC failure" >&2
	exit 1
}

printf 'small' >"$work/source.txt"
mount -o remount,ro -- "$mountpoint"
set +e
printf 'y\n' | XDG_STATE_HOME="$work/state" COPY_RS_DISABLE_ETA_PRIORS=1 "$copy_bin" "$work/source.txt" "$mountpoint/destination.txt"
readonly_rc=$?
set -e
[[ $readonly_rc -ne 0 ]] || {
	echo "expected read-only failure" >&2
	exit 1
}
mount -o remount,rw -- "$mountpoint"

# Simulate removal after preview by detaching the loop while the process waits
# for confirmation. The operation must fail and retain its journal.
coproc COPY_PROC { XDG_STATE_HOME="$work/state" COPY_RS_DISABLE_ETA_PRIORS=1 "$copy_bin" "$work/source.txt" "$mountpoint/removed.txt"; }
sleep 1
umount -- "$mountpoint"
losetup -d -- "$loopdev"
loopdev=
printf 'y\n' >&"${COPY_PROC[1]}"
set +e
wait "$COPY_PROC_PID"
removed_rc=$?
set -e
[[ $removed_rc -ne 0 ]] || {
	echo "expected removed-device failure" >&2
	exit 1
}

echo "copy Linux ENOSPC/read-only/device-removal fault tests passed"
