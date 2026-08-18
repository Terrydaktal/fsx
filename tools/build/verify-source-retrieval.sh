#!/usr/bin/env bash
set -euo pipefail

artifact_dir="${1:?usage: verify-source-retrieval.sh ARTIFACT_DIR}"
artifact_dir="$(readlink -f -- "$artifact_dir" 2>/dev/null || realpath -- "$artifact_dir")"
tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/fsx-source-check.XXXXXX")"
trap 'rm -rf -- "$tmp_root"' EXIT

tools/build/retrieve-source.sh "$artifact_dir" "$tmp_root/source"
found_manifest=false
for manifest in "$tmp_root/source"/*/Cargo.toml; do
	if [[ -f "$manifest" ]]; then
		found_manifest=true
		break
	fi
done
[[ "$found_manifest" == true ]] || { echo "retrieved source is missing Cargo.toml" >&2; exit 1; }
echo "source retrieval passed: $artifact_dir"
