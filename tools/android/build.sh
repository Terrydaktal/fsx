#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
FRIZ_DIR=${FRIZ_DIR:-"$ROOT_DIR/../friz"}
FRIZ_TARGET_DIR=${FRIZ_CARGO_TARGET_DIR:-"$FRIZ_DIR/target"}
TARGET=${ANDROID_TARGET:-aarch64-linux-android}
PROFILE_DIR=${CARGO_TARGET_DIR:-"$ROOT_DIR/target"}
MODE=${1:-build}

if ! rustup target list --installed | grep -qx "$TARGET"; then
	if [[ "$MODE" == "check" ]]; then
		echo "Rust target $TARGET is not installed; run: rustup target add $TARGET" >&2
		exit 1
	fi
	rustup target add "$TARGET"
fi

if [[ ! -f "$FRIZ_DIR/Cargo.toml" ]]; then
	echo "friz checkout not found at $FRIZ_DIR" >&2
	exit 1
fi

build_fsx() {
	if [[ "$MODE" == "check" ]]; then
		cargo check --manifest-path "$ROOT_DIR/Cargo.toml" --target "$TARGET" \
			-p tree -p unearth -p twig -p copy-rs --no-default-features
	elif command -v cargo-ndk >/dev/null 2>&1; then
		cargo ndk -t arm64-v8a build --release --manifest-path "$ROOT_DIR/Cargo.toml" \
			-p tree -p unearth -p twig -p copy-rs --no-default-features
	else
		: "${CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER:?Install cargo-ndk or set the Android linker}"
		cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml" \
			--target "$TARGET" -p tree -p unearth -p twig -p copy-rs --no-default-features
	fi
}

build_friz() {
	if [[ "$MODE" == "check" ]]; then
		CARGO_TARGET_DIR="$FRIZ_TARGET_DIR" cargo check --manifest-path "$FRIZ_DIR/Cargo.toml" \
			--target "$TARGET" --no-default-features
	elif command -v cargo-ndk >/dev/null 2>&1; then
		CARGO_TARGET_DIR="$FRIZ_TARGET_DIR" cargo ndk -t arm64-v8a build --release \
			--manifest-path "$FRIZ_DIR/Cargo.toml" --no-default-features
	else
		: "${CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER:?Install cargo-ndk or set the Android linker}"
		CARGO_TARGET_DIR="$FRIZ_TARGET_DIR" cargo build --release \
			--manifest-path "$FRIZ_DIR/Cargo.toml" --target "$TARGET" --no-default-features
	fi
}

build_fsx
build_friz

if [[ "$MODE" == "build" ]]; then
	echo "Android binaries built for $TARGET."
	echo "fsx: $PROFILE_DIR/$TARGET/release/{tree,unearth,twig,copy-rs}"
	echo "friz: $FRIZ_TARGET_DIR/$TARGET/release/friz"
fi
