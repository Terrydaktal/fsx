#!/usr/bin/env bash
set -euo pipefail

# Validate symbols as a consumer would see them on a clean machine: only the
# stripped executable and its matching debuglink file are made available.
artifact_dir="${1:?usage: verify-symbolization.sh ARTIFACT_DIR}"
artifact_dir="$(readlink -f -- "$artifact_dir" 2>/dev/null || realpath -- "$artifact_dir")"
manifest="$artifact_dir/manifest.tsv"
[[ -f "$manifest" ]] || { echo "missing artifact manifest: $manifest" >&2; exit 1; }

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/fsx-symbols.XXXXXX")"
trap 'rm -rf -- "$tmp_root"' EXIT

while IFS=$'\t' read -r binary build_id _sha debug_rel; do
	[[ "$binary" == "binary" ]] && continue
	input="$artifact_dir/bin/$binary"
	debug="$artifact_dir/$debug_rel"
	[[ -x "$input" && -f "$debug" ]] || { echo "missing $binary artifact pair" >&2; exit 1; }
	actual_id="$(readelf -n "$input" | awk '/Build ID/ {print $NF; exit}')"
	[[ "$actual_id" == "$build_id" ]] || { echo "Build ID mismatch for $binary" >&2; exit 1; }
	debug_name="$(readelf --string-dump=.gnu_debuglink "$input" | awk '/\] / {print $NF; exit}')"
	[[ -n "$debug_name" ]] || { echo "missing GNU debuglink for $binary" >&2; exit 1; }
	isolated="$tmp_root/$binary"
	mkdir -p -- "$isolated"
	cp -- "$input" "$isolated/$binary"
	cp -- "$debug" "$isolated/$debug_name"
	address="$(readelf --debug-dump=decodedline "$debug" 2>/dev/null |
		awk '{for (i = 1; i <= NF; i++) if ($i ~ /^0x[0-9a-f]+$/) {print $i; exit}}')"
	[[ -n "$address" ]] || { echo "unable to locate a debug line in $binary" >&2; exit 1; }
	resolved="$(env -i PATH=/usr/bin:/bin addr2line -e "$isolated/$binary" -fip "$address")"
	if [[ "$resolved" == *"??:??"* || "$resolved" == *"??:0"* ]]; then
		echo "clean-machine symbolization failed for $binary: $resolved" >&2
		exit 1
	fi
	done < "$manifest"

echo "clean-machine symbolization passed: $artifact_dir"
