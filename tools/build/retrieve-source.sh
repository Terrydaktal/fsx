#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
	echo "usage: $0 ARTIFACT_DIR OUTPUT_DIR" >&2
	exit 2
fi

artifact_dir="$(readlink -f -- "$1" 2>/dev/null || realpath -- "$1")"
output_dir="$2"
archive="$artifact_dir/source.tar.gz"
[[ -f "$archive" ]] || { echo "missing source archive: $archive" >&2; exit 1; }
[[ ! -e "$output_dir" ]] || { echo "output directory already exists: $output_dir" >&2; exit 1; }

while IFS= read -r entry; do
	case "$entry" in
		/*|../*|*/../*|*/..)
			echo "unsafe source archive entry: $entry" >&2
			exit 1
			;;
	esac
done < <(tar -tzf "$archive")

mkdir -p -- "$output_dir"
tar -xzf "$archive" --no-same-owner --no-same-permissions -C "$output_dir"
printf 'source extracted: %s\n' "$output_dir"
