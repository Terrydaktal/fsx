#!/usr/bin/env bash
set -euo pipefail

# Build optimized, stripped executables while retaining matching private
# debuginfo indexed by each ELF Build ID. The normal target/release profile is
# unchanged; this produces a reproducible post-mortem artifact set beside it.

script_path="$(readlink -f -- "${BASH_SOURCE[0]}" 2>/dev/null || realpath -- "${BASH_SOURCE[0]}")"
repo_root="$(cd -- "$(dirname -- "$script_path")/../.." && pwd)"
profile="${FSX_RELEASE_PROFILE:-release-symbols}"
artifact_root="${FSX_ARTIFACT_ROOT:-$repo_root/target/fsx-artifacts}"

cd -- "$repo_root"
# Build the deployable feature set. Diagnostic-only hooks are intentionally
# excluded; CI builds those in a separate diagnostic profile for fault tests.
cargo build --locked --workspace --all-targets --profile "$profile"

release_dir="$repo_root/target/$profile"
build_id="$(git rev-parse HEAD 2>/dev/null || printf '%s' unknown)"
output_dir="$artifact_root/$build_id"
mkdir -p -- "$output_dir/bin" "$output_dir/debug"
manifest="$output_dir/manifest.tsv"
printf 'binary\tbuild_id\tsha256\tdebug_symbol\n' >"$manifest"

for binary in tree twig unearth fsxd copy-rs; do
	input="$release_dir/$binary"
	if [[ ! -x "$input" ]]; then
		echo "missing release binary: $input" >&2
		exit 1
	fi
	id="$(readelf -n "$input" | awk '/Build ID/ {print $NF; exit}')"
	[[ -n "$id" ]] || { echo "missing Build ID: $input" >&2; exit 1; }
	debug="$output_dir/debug/$binary-$id.debug"
	stripped="$output_dir/bin/$binary"
	objcopy --only-keep-debug "$input" "$debug"
	cp --reflink=auto --preserve=mode,timestamps -- "$input" "$stripped"
	strip --strip-debug --strip-unneeded "$stripped"
	objcopy --add-gnu-debuglink="$debug" "$stripped"
	sha="$(sha256sum "$stripped" | awk '{print $1}')"
	printf '%s\t%s\t%s\t%s\n' "$binary" "$id" "$sha" "debug/$binary-$id.debug" >>"$manifest"
done

dirty=false
if ! git diff --quiet || [[ -n "$(git ls-files --others --exclude-standard)" ]]; then
	dirty=true
fi
printf 'source_commit=%s\nsource_dirty=%s\nprofile=%s\nrustc=%s\n' \
	"$build_id" "$dirty" "$profile" "$(rustc --version)" >"$output_dir/build-info.txt"
# Keep an exact, read-only source snapshot beside the binaries. This makes
# post-mortem source retrieval independent of the build machine's checkout.
if [[ "$dirty" == true ]]; then
	git ls-files -co --exclude-standard -z |
		tar --null --files-from=- --transform="s,^,fsx-$build_id/," -czf "$output_dir/source.tar.gz"
else
	git archive --format=tar.gz --prefix="fsx-$build_id/" "$build_id" >"$output_dir/source.tar.gz"
fi
printf 'release artifacts: %s\n' "$output_dir"
