# Android builds and smoke tests

These scripts build the standalone ARM64 tools without `fsxd` and without the
index features. They require the sibling `friz` checkout (and its
`fuzzy-rank` path dependency).

Install the Android Rust target and either `cargo-ndk` or an Android NDK linker,
then run:

```sh
tools/android/build.sh check   # cross-target type/build check
tools/android/build.sh         # release binaries
tools/android/smoke.sh         # rooted adb device smoke test
```

The smoke test expects a conventional user-invoked root command named `sudo`.
Set `FSX_ANDROID_ROOT_CMD` when the phone uses a different wrapper. It creates
only `/data/local/tmp/fsx-smoke`, verifies Tree, Unearth live mode, Twig,
Copy, and the absence of SQLite files, and removes that temporary directory on
exit. Friz is built and pushed, but is not automatically driven because its UI
requires an interactive `/dev/tty`.

The no-default-features builds are deliberate: Unearth and Twig then omit their
optional SQLite/index dependencies. Desktop builds retain their normal indexed
features through the workspace's explicit `--all-features` CI commands.
