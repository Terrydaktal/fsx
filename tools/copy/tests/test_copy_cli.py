#!/usr/bin/env python3
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
COPY_BIN = ROOT / "copy"
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
BACKUP_SUFFIX_RE = re.compile(r"^\d{8}-\d{6}(?:\.\d+)?$")


def strip_ansi(text):
    return ANSI_RE.sub("", text)


def run_copy(args, cwd=None, confirm=False, env=None):
    merged_env = os.environ.copy()
    # Integration tests must not mutate the user's persisted ETA priors.
    merged_env.setdefault("COPY_RS_DISABLE_ETA_PRIORS", "1")
    if env:
        merged_env.update(env)
    proc = subprocess.run(
        [str(COPY_BIN), *args],
        cwd=str(cwd) if cwd else None,
        input=("y\n" if confirm else "n\n"),
        text=True,
        capture_output=True,
        env=merged_env,
    )
    combined = f"{proc.stdout}\n{proc.stderr}".strip()
    return proc.returncode, strip_ansi(combined), proc.stdout


def write_file(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def find_backups(parent, base_name):
    found = []
    if not parent.exists():
        return found
    prefix = f"{base_name}."
    for child in parent.iterdir():
        name = child.name
        if not name.startswith(prefix):
            continue
        suffix = name[len(prefix):]
        if BACKUP_SUFFIX_RE.match(suffix):
            found.append(child)
    return sorted(found, key=lambda p: p.name)


class CopyCliIntegrationTests(unittest.TestCase):
    def test_help_includes_expected_aliases(self):
        rc, out, _ = run_copy(["--help"])
        self.assertEqual(rc, 0)
        self.assertIn("-m, --move", out)
        self.assertIn("-s, --sudo", out)
        self.assertIn("-c, --contents-only", out)
        self.assertIn("--verbose", out)
        self.assertIn("--showall", out)
        self.assertIn("--collision policy", out)
        self.assertIn("Default: source:size-differs", out)
        self.assertIn("--collision source:newer,larger", out)
        self.assertIn("--collision dest:newer+larger", out)
        self.assertIn("--sync", out)
        self.assertIn("--create-destination-parents", out)

    def test_preview_shows_policy_independent_file_relation_breakdown(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst" / "A"

            write_file(src / "same.txt", "same\n")
            write_file(dst / "same.txt", "same\n")
            same_ts = 1_700_000_000
            os.utime(src / "same.txt", (same_ts, same_ts))
            os.utime(dst / "same.txt", (same_ts, same_ts))

            write_file(src / "newer_same_size.txt", "same-size\n")
            write_file(dst / "newer_same_size.txt", "same-size\n")
            older = 1_700_000_010
            newer = older + 100
            os.utime(src / "newer_same_size.txt", (newer, newer))
            os.utime(dst / "newer_same_size.txt", (older, older))

            write_file(src / "newer_larger.txt", "source-is-larger\n")
            write_file(dst / "newer_larger.txt", "dst\n")
            os.utime(src / "newer_larger.txt", (newer + 100, newer + 100))
            os.utime(dst / "newer_larger.txt", (older + 50, older + 50))

            rc, out, _ = run_copy(
                [str(src), str(dst), "-c", "--preview", "--collision", "dest:always"],
            )
            self.assertEqual(rc, 0, out)
            self.assertIn("Time=Size=", out)
            self.assertIn("Time+Size=", out)
            self.assertIn("Time+Size+", out)
            self.assertRegex(
                out,
                r"Files\s+\|\s*0\s+\|\s*0\s+\|\s*0\s+\|\s*0\s+\|\s*1\s+\|\s*0",
            )
            self.assertRegex(
                out,
                r"Files\s+\|\s*0\s+\|\s*0\s+\|\s*0\s+\|\s*0\s+\|\s*1\s+\|\s*0\s+\|\s*0",
                )

    def test_copy_fails_preflight_when_destination_space_is_insufficient(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src.bin"
            dst = Path(td) / "dst"
            dst.mkdir(parents=True, exist_ok=True)
            vfs = os.statvfs(dst)
            required_bytes = (vfs.f_bavail * vfs.f_frsize) + 1
            with src.open("wb") as fh:
                try:
                    fh.truncate(required_bytes)
                except OSError as exc:
                    self.skipTest(f"cannot create sparse preflight file at {required_bytes} bytes: {exc}")

            rc, out, _ = run_copy([str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 1, out)
            self.assertIn("Insufficient free space on destination filesystem", out)

    def test_move_fast_rename_is_not_blocked_by_space_preflight(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "huge.bin"
            dst = Path(td) / "renamed.bin"
            vfs = os.statvfs(td)
            required_bytes = (vfs.f_bavail * vfs.f_frsize) + 1
            with src.open("wb") as fh:
                try:
                    fh.truncate(required_bytes)
                except OSError as exc:
                    self.skipTest(f"cannot create sparse rename file at {required_bytes} bytes: {exc}")

            rc, out, _ = run_copy(["--move", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse(src.exists(), out)
            self.assertTrue(dst.exists(), out)
            self.assertIn("Fast-path rename on same filesystem", out)

    def test_move_same_slot_to_parent_is_noop_by_default(self):
        with tempfile.TemporaryDirectory() as td:
            base = Path(td) / "Telegram Backup" / "poo"
            write_file(base / "poo" / "inner.txt", "x\n")
            rc, out, _ = run_copy(["--move", "poo", ".."], cwd=base)
            self.assertEqual(rc, 0)
            self.assertIn("No changes detected; nothing to move.", out)

    def test_move_same_slot_to_parent_with_contents_only_plans_merge(self):
        with tempfile.TemporaryDirectory() as td:
            base = Path(td) / "Telegram Backup" / "poo"
            write_file(base / "poo" / "sdf", "x\n")
            rc, out, _ = run_copy(["--move", "poo", "..", "-c", "-v"], cwd=base)
            self.assertEqual(rc, 0)
            self.assertIn("Cancelled.", out)
            self.assertTrue((base / "poo" / "sdf").exists(), out)
            self.assertFalse((base.parent / "poo" / "sdf").exists(), out)

    def test_move_same_slot_to_parent_with_contents_only_and_overwrite_is_not_noop(self):
        with tempfile.TemporaryDirectory() as td:
            base = Path(td) / "Telegram Backup" / "poo"
            write_file(base / "poo" / "sdf", "x\n")
            rc, out, _ = run_copy(["--move", "poo", "..", "-c", "-o", "-v"], cwd=base)
            self.assertEqual(rc, 0)
            self.assertIn("Cancelled.", out)
            self.assertTrue((base / "poo" / "sdf").exists(), out)
            self.assertFalse((base.parent / "poo" / "sdf").exists(), out)

    def test_copy_directory_default_nests_under_destination(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "file.txt", "payload\n")
            dst.mkdir(parents=True)
            rc, out, _ = run_copy([str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "A" / "file.txt").exists())

    def test_create_destination_parents_allows_nested_exact_directory_target(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "Users"
            dst = Path(td) / "backup" / "2022PCWindowsBackup" / "Users"
            write_file(src / "Public" / "desktop.ini", "desktop\n")

            rc, out, _ = run_copy(
                [str(src), str(dst), "--create-destination-parents"],
                confirm=True,
            )
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "Public" / "desktop.ini").exists(), out)
            self.assertFalse((dst / "Users").exists(), out)

    def test_copy_directory_contents_only_merges_into_destination(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "file.txt", "payload\n")
            dst.mkdir(parents=True)
            rc, out, _ = run_copy([str(src), str(dst), "-c"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "file.txt").exists())
            self.assertFalse((dst / "A").exists())

    def test_copy_file_into_existing_directory_succeeds(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "print_extension_groups.sh"
            dst = Path(td) / "config" / "fish"
            write_file(src, "echo hi\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy([str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "print_extension_groups.sh").exists(), out)

    def test_rust_backend_reports_failed_path_and_os_error(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "file.txt", "payload\n")
            (dst / "A" / "file.txt").mkdir(parents=True)

            rc, out, _ = run_copy([str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 1, out)
            self.assertIn("Rust backend failure:", out)
            self.assertIn(str(src / "file.txt"), out)
            self.assertIn("Is a directory", out)

    def test_copy_preserves_literal_backslash_in_unix_filename(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            filename = "notes\\Oscillators with RC Feedback Circuits.pdf"
            write_file(src / filename, "payload\n")

            rc, out, _ = run_copy([str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / filename).is_file(), out)

    def test_sync_mode_deletes_destination_only_entries(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "keep.txt", "new\n")
            write_file(dst / "A" / "keep.txt", "old\n")
            write_file(dst / "A" / "only-dst.txt", "remove-me\n")

            rc, out, _ = run_copy([str(src), str(dst), "--sync"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Sync", out)
            self.assertIn("Starting copy (rust backend)", out)
            self.assertTrue((dst / "A" / "keep.txt").exists(), out)
            self.assertFalse((dst / "A" / "only-dst.txt").exists(), out)

    def test_sync_mode_copies_same_size_files_when_mtime_differs(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "source-newer.txt", "source\n")
            write_file(dst / "A" / "source-newer.txt", "target\n")
            write_file(src / "source-older.txt", "source\n")
            write_file(dst / "A" / "source-older.txt", "target\n")

            base = 1_700_000_000
            os.utime(src / "source-newer.txt", (base + 20, base + 20))
            os.utime(dst / "A" / "source-newer.txt", (base, base))
            os.utime(src / "source-older.txt", (base, base))
            os.utime(dst / "A" / "source-older.txt", (base + 20, base + 20))

            rc, out, _ = run_copy([str(src), str(dst), "--sync"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Starting copy (rust backend)", out)
            self.assertEqual((dst / "A" / "source-newer.txt").read_text(), "source\n")
            self.assertEqual((dst / "A" / "source-older.txt").read_text(), "source\n")

    def test_sync_mode_replaces_type_conflicts_and_nested_extras(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "source-dir" / "inside.txt", "inside\n")
            write_file(src / "source-file", "file\n")
            write_file(dst / "A" / "source-dir", "old-file\n")
            write_file(dst / "A" / "source-file" / "nested.txt", "old-dir\n")
            write_file(dst / "A" / "stale" / "nested" / "old.txt", "remove\n")

            rc, out, _ = run_copy([str(src), str(dst), "--sync"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "A" / "source-dir").is_dir(), out)
            self.assertEqual((dst / "A" / "source-dir" / "inside.txt").read_text(), "inside\n")
            self.assertTrue((dst / "A" / "source-file").is_file(), out)
            self.assertEqual((dst / "A" / "source-file").read_text(), "file\n")
            self.assertFalse((dst / "A" / "stale").exists(), out)
            self.assertFalse(
                any((dst / "A").glob(".copy-rs-partial-*")),
                "atomic sync staging files should not remain after success",
            )

    def test_sync_mode_contents_maps_source_children_directly_to_destination(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src"
            dst = Path(td) / "dst"
            write_file(src / "keep" / "source.txt", "source\n")
            write_file(dst / "keep" / "source.txt", "old\n")
            write_file(dst / "stale.txt", "remove\n")

            rc, out, _ = run_copy([str(src), str(dst), "--sync", "-c"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertEqual((dst / "keep" / "source.txt").read_text(), "source\n")
            self.assertFalse((dst / "stale.txt").exists(), out)
            self.assertFalse((dst / "src").exists(), out)

    def test_sync_mode_deletes_destination_only_symlink(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "keep.txt", "same\n")
            write_file(dst / "A" / "keep.txt", "same\n")
            os.symlink("missing-target", dst / "A" / "stale-link")

            rc, out, _ = run_copy([str(src), str(dst), "--sync"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse((dst / "A" / "stale-link").is_symlink(), out)
            self.assertEqual((dst / "A" / "keep.txt").read_text(), "same\n")

    def test_sync_mode_with_backup_creates_snapshot_before_delete_sync(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "keep.txt", "new\n")
            write_file(dst / "A" / "keep.txt", "old\n")
            write_file(dst / "A" / "only-dst.txt", "remove-me\n")

            rc, out, _ = run_copy([str(src), str(dst), "--sync", "-b"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Backup saved as:", out)
            self.assertFalse((dst / "A" / "only-dst.txt").exists(), out)
            backups = find_backups(dst, "A")
            self.assertEqual(len(backups), 1, f"unexpected backups: {backups}")
            self.assertTrue((backups[0] / "only-dst.txt").exists(), out)

    def test_sync_mode_conflicts_with_overwrite(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "f.txt", "x\n")
            dst.mkdir(parents=True, exist_ok=True)
            rc, out, _ = run_copy([str(src), str(dst), "--sync", "-o"])
            self.assertEqual(rc, 1, out)
            self.assertIn("--sync cannot be combined with --overwrite", out)

    def test_sync_mode_conflicts_with_move(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "f.txt", "x\n")
            dst.mkdir(parents=True, exist_ok=True)
            rc, out, _ = run_copy([str(src), str(dst), "--sync", "--move"])
            self.assertEqual(rc, 1, out)
            self.assertIn("--sync currently supports copy mode only", out)

    def test_sync_mode_rejects_file_source(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src.txt"
            dst = Path(td) / "dst"
            write_file(src, "x\n")
            dst.mkdir(parents=True, exist_ok=True)
            rc, out, _ = run_copy([str(src), str(dst), "--sync"])
            self.assertEqual(rc, 1, out)
            self.assertIn("--sync currently supports directory sources only", out)

    def test_copy_multiple_files_into_existing_directory_succeeds(self):
        with tempfile.TemporaryDirectory() as td:
            src1 = Path(td) / "one.mkv"
            src2 = Path(td) / "two.mkv"
            dst = Path(td) / "Videos"
            write_file(src1, "a\n")
            write_file(src2, "b\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, raw = run_copy([str(src1), str(src2), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "one.mkv").exists(), out)
            self.assertTrue((dst / "two.mkv").exists(), out)
            self.assertRegex(raw, rf"\x1b\[93m{re.escape(str(dst))}/\x1b\[0m")
            self.assertRegex(raw, r"\x1b\[92mone\.mkv\x1b\[0m")
            self.assertRegex(raw, r"\x1b\[92mtwo\.mkv\x1b\[0m")

    def test_move_multiple_files_same_fs_uses_batch_fast_rename(self):
        with tempfile.TemporaryDirectory() as td:
            src1 = Path(td) / "one.mkv"
            src2 = Path(td) / "two.mkv"
            dst = Path(td) / "Videos"
            write_file(src1, "a\n")
            write_file(src2, "b\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy(["--move", str(src1), str(src2), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse(src1.exists(), out)
            self.assertFalse(src2.exists(), out)
            self.assertTrue((dst / "one.mkv").exists(), out)
            self.assertTrue((dst / "two.mkv").exists(), out)
            self.assertIn("Fast-path rename on same filesystem (batch)", out)
            self.assertNotIn("Starting cleanup", out)

    def test_move_file_into_dir_without_fastpath_still_deletes_source(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "clip.mkv"
            dst = Path(td) / "Videos"
            write_file(src, "new-content\n")
            write_file(dst / "clip.mkv", "old\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse(src.exists(), out)
            self.assertTrue((dst / "clip.mkv").exists(), out)

    def test_move_multiple_identical_files_performs_cleanup_not_noop(self):
        with tempfile.TemporaryDirectory() as td:
            src1 = Path(td) / "one.mkv"
            src2 = Path(td) / "two.mkv"
            dst = Path(td) / "Videos"
            write_file(src1, "same-a\n")
            write_file(src2, "same-b\n")
            write_file(dst / "one.mkv", "same-a\n")
            write_file(dst / "two.mkv", "same-b\n")

            rc, out, _ = run_copy(["--move", str(src1), str(src2), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse(src1.exists(), out)
            self.assertFalse(src2.exists(), out)
            self.assertIn("Destination already has matching files", out)
            self.assertIn("Starting cleanup", out)
            self.assertRegex(out, r"Files\s+\|\s*0\s+\|\s*0\s+\|\s*2\s+\|\s*0\s+\|\s*2\s+\|\s*0")

    def test_contents_only_new_named_target_preview_roots_at_target_dir(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "Movies"
            dst_parent = Path(td) / "dst" / "mate 20x"
            dst_target = dst_parent / "Movies"
            write_file(src / "Telegram" / "img.jpg", "img\n")
            write_file(src / "clip.mp4", "vid\n")
            write_file(dst_parent / "keep.txt", "keep\n")

            rc, out, raw = run_copy(["--move", str(src), str(dst_target), "-c"])
            self.assertEqual(rc, 0, out)
            path_lines = [ln for ln in out.splitlines() if ln.startswith(str(Path(td)))]
            self.assertTrue(path_lines, out)
            self.assertEqual(path_lines[0], f"{dst_target}/", out)
            self.assertIn("Telegram/", out)
            self.assertIn("clip.mp4", out)
            self.assertIn(f"{dst_parent}/\x1b[92mMovies/\x1b[0m", raw)

    def test_directory_new_named_target_preview_roots_at_target_dir(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "phone" / "Internal storage" / "Movies"
            dst_parent = Path(td) / "backup" / "mate 20x"
            dst_target = dst_parent / "Movies"
            write_file(src / "Telegram" / "img.jpg", "img\n")
            write_file(src / "clip.mp4", "vid\n")
            write_file(dst_parent / "keep.txt", "keep\n")

            rc, out, raw = run_copy(["--move", str(src), str(dst_target)])
            self.assertEqual(rc, 0, out)
            path_lines = [ln for ln in out.splitlines() if ln.startswith(str(Path(td)))]
            self.assertTrue(path_lines, out)
            self.assertEqual(path_lines[0], f"{dst_target}/", out)
            self.assertIn("Telegram/", out)
            self.assertIn("clip.mp4", out)
            self.assertIn(f"{dst_parent}/\x1b[92mMovies/\x1b[0m", raw)

    def test_move_directory_contents_only_merges_and_removes_nested_source(self):
        with tempfile.TemporaryDirectory() as td:
            base = Path(td) / "Telegram Backup" / "poo"
            parent = base.parent
            write_file(base / "poo" / "sdf", "hello\n")
            write_file(base / "keep.txt", "keep\n")
            rc, out, _ = run_copy(["--move", "poo", "..", "-c"], cwd=base, confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((parent / "sdf").exists())
            self.assertFalse((base / "poo").exists())
            self.assertTrue((base / "keep.txt").exists())

    def test_move_contents_only_existing_dest_uses_premerge_fast_rename_for_noncolliders(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src"
            dst = Path(td) / "dst"
            write_file(src / "README.md", "new-readme\n")
            write_file(src / "copyq_script_override.js", "override\n")
            write_file(src / "scripts" / "a.sh", "echo hi\n")
            write_file(dst / "README.md", "old\n")
            write_file(dst / "keep.txt", "keep\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst), "-c"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Fast-path pre-merge rename:", out)
            self.assertFalse(src.exists(), out)
            self.assertEqual((dst / "README.md").read_text(encoding="utf-8"), "new-readme\n")
            self.assertEqual(
                (dst / "copyq_script_override.js").read_text(encoding="utf-8"), "override\n"
            )
            self.assertEqual((dst / "scripts" / "a.sh").read_text(encoding="utf-8"), "echo hi\n")
            self.assertEqual((dst / "keep.txt").read_text(encoding="utf-8"), "keep\n")

    def test_move_current_directory_into_existing_child_uses_contents_semantics(self):
        with tempfile.TemporaryDirectory() as td:
            videos = Path(td) / "Videos"
            videos.mkdir(parents=True, exist_ok=True)
            write_file(videos / "clip1.mkv", "one\n")
            write_file(videos / "clip2.mkv", "two\n")
            write_file(videos / "obs" / "keep.txt", "keep\n")

            rc, out, _ = run_copy(["--move", ".", "obs"], cwd=videos, confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse((videos / "clip1.mkv").exists(), out)
            self.assertFalse((videos / "clip2.mkv").exists(), out)
            self.assertTrue((videos / "obs" / "clip1.mkv").exists(), out)
            self.assertTrue((videos / "obs" / "clip2.mkv").exists(), out)
            self.assertTrue((videos / "obs" / "keep.txt").exists(), out)
            self.assertFalse((videos / "obs" / "obs").exists(), out)

    def test_move_merge_identical_destination_still_removes_source(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst_root = Path(td) / "dst"
            dst = dst_root / "A"
            write_file(src / "same.txt", "payload\n")
            write_file(dst / "same.txt", "payload\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst_root)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertRegex(out, r"Files\s+\|\s*0\s+\|\s*0\s+\|\s*1\s+\|\s*0\s+\|\s*1\s+\|\s*0")
            self.assertFalse(src.exists(), out)
            self.assertTrue((dst / "same.txt").exists(), out)
            self.assertIn("Starting move cleanup:", out)
            self.assertNotIn("Starting move (", out)
            self.assertNotIn("Progress: ---%", out)
            self.assertIn("Delete", out)
            self.assertIn("Cleanup Duration:", out)
            self.assertIn("Cleanup Flush Duration:", out)
            self.assertIn("Total Duration:", out)

    def test_move_contents_only_transfers_symlink_and_removes_source(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "target.txt", "payload\n")
            src.mkdir(parents=True, exist_ok=True)
            os.symlink("target.txt", src / "link.txt")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy(["--move", str(src), str(dst), "-c"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse(src.exists(), out)
            self.assertTrue((dst / "target.txt").is_file(), out)
            self.assertTrue((dst / "link.txt").is_symlink(), out)
            self.assertEqual(os.readlink(dst / "link.txt"), "target.txt")

    def test_copy_replace_dest_symlink_flag_replaces_link_itself(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src.txt"
            dst_target = Path(td) / "dest-target.txt"
            dst_link = Path(td) / "dest-link.txt"
            write_file(src, "new\n")
            write_file(dst_target, "old\n")
            os.symlink(dst_target.name, dst_link)

            rc, out, _ = run_copy(["--replace-dest-symlink", str(src), str(dst_link)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse(dst_link.is_symlink(), out)
            self.assertEqual(dst_link.read_text(encoding="utf-8"), "new\n")
            self.assertEqual(dst_target.read_text(encoding="utf-8"), "old\n")

    def test_copy_preserves_directory_mtime(self):
        with tempfile.TemporaryDirectory() as td:
            src_root = Path(td) / "src" / "tree"
            dst_root = Path(td) / "dst"
            write_file(src_root / "sub" / "file.txt", "payload\n")
            src_target_dir = src_root / "sub"
            target_ts = 1_700_000_000
            os.utime(src_target_dir, (target_ts, target_ts))

            rc, out, _ = run_copy([str(src_root), str(dst_root)], confirm=True)
            self.assertEqual(rc, 0, out)

            dst_target_dir = dst_root / "sub"
            self.assertTrue(dst_target_dir.is_dir(), out)
            src_mtime = int(src_target_dir.stat().st_mtime)
            dst_mtime = int(dst_target_dir.stat().st_mtime)
            self.assertEqual(dst_mtime, src_mtime, out)

    def test_copy_preserves_file_and_directory_atime(self):
        with tempfile.TemporaryDirectory() as td:
            src_root = Path(td) / "src" / "tree"
            dst_root = Path(td) / "dst"
            file_path = src_root / "sub" / "file.txt"
            write_file(file_path, "payload\n")

            file_ts = 1_700_000_100
            dir_ts = 1_700_000_200
            os.utime(file_path, (file_ts, file_ts))
            os.utime(src_root / "sub", (dir_ts, dir_ts))

            rc, out, _ = run_copy([str(src_root), str(dst_root)], confirm=True)
            self.assertEqual(rc, 0, out)

            dst_file = dst_root / "sub" / "file.txt"
            dst_dir = dst_root / "sub"
            self.assertEqual(int(dst_file.stat().st_atime), file_ts, out)
            self.assertEqual(int(dst_dir.stat().st_atime), dir_ts, out)

    def test_move_cleanup_only_removes_identical_symlink_source(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst_root = Path(td) / "dst"
            dst = dst_root / "A"
            write_file(src / "target.txt", "payload\n")
            src.mkdir(parents=True, exist_ok=True)
            os.symlink("target.txt", src / "link.txt")
            write_file(dst / "target.txt", "payload\n")
            dst.mkdir(parents=True, exist_ok=True)
            os.symlink("target.txt", dst / "link.txt")

            rc, out, _ = run_copy(["--move", str(src), str(dst_root)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Starting move cleanup:", out)
            self.assertFalse(src.exists(), out)
            self.assertTrue((dst / "link.txt").is_symlink(), out)
            self.assertEqual(os.readlink(dst / "link.txt"), "target.txt")

    def test_move_symlink_only_cleanup_preview_shows_deleted_source_count(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst_root = Path(td) / "dst"
            dst = dst_root / "A"
            src.mkdir(parents=True, exist_ok=True)
            dst.mkdir(parents=True, exist_ok=True)
            os.symlink("target.txt", src / "link.txt")
            os.symlink("target.txt", dst / "link.txt")

            rc, out, _ = run_copy(["--move", str(src), str(dst_root)])
            self.assertEqual(rc, 0, out)
            self.assertIn("Del(src)", out)

    def test_move_contents_only_named_target_removes_source_dir_after_merge(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td) / "Phone" / "mate 20x"
            src = root / "Camera new" / "Camera"
            dst = root / "Camera"
            write_file(src / "sub" / "same1.jpg", "same1\n")
            write_file(src / "sub" / "same2.jpg", "same2\n")
            write_file(src / "sub" / "new1.jpg", "new1\n")
            write_file(dst / "sub" / "same1.jpg", "same1\n")
            write_file(dst / "sub" / "same2.jpg", "same2\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst), "-c"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertFalse(src.exists(), out)
            self.assertTrue((dst / "sub" / "same1.jpg").exists(), out)
            self.assertTrue((dst / "sub" / "same2.jpg").exists(), out)
            self.assertTrue((dst / "sub" / "new1.jpg").exists(), out)

    def test_overwrite_nested_target_replaces_existing_directory(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "poo"
            dst = Path(td) / "dst" / "root" / "poo"
            write_file(src / "new.txt", "new\n")
            write_file(dst / "old.txt", "old\n")
            rc, out, _ = run_copy(["--move", "-o", str(src), str(dst.parent)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "new.txt").exists())
            self.assertFalse((dst / "old.txt").exists())

    def test_overwrite_explicit_destination_with_contents_only_replaces_path(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst" / "B"
            write_file(src / "new.txt", "new\n")
            write_file(dst / "old.txt", "old\n")
            rc, out, _ = run_copy(["--move", "-o", "-c", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertTrue((dst / "new.txt").exists())
            self.assertFalse((dst / "old.txt").exists())

    def test_overwrite_preview_shows_old_new_pair(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "poo"
            dst = Path(td) / "dst" / "root" / "poo"
            write_file(src / "new.txt", "new\n")
            write_file(dst / "old.txt", "old\n")
            rc, out, _ = run_copy(["--move", "-o", str(src), str(dst.parent), "-v"])
            self.assertEqual(rc, 0)
            self.assertIn("poo/ (old)", out)
            self.assertIn("poo/ (new)", out)
            self.assertIn("Del(dest)", out)

    def test_dir_rename_preview_does_not_flatten_children_into_parent(self):
        with tempfile.TemporaryDirectory() as td:
            parent = Path(td) / "Telegram Backup"
            src = parent / "g"
            dst = parent / "Sensitive Information 5"
            write_file(src / "css" / "x.css", "x\n")
            write_file(src / "messages.html", "m\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst)])
            self.assertEqual(rc, 0, out)
            self.assertIn(str(parent) + "/", out)
            self.assertIn("Sensitive Information 5/", out)
            self.assertNotIn("\n├── css/", out)
            self.assertNotIn("\n└── css/", out)

    def test_move_same_parent_rename_shows_removed_source(self):
        with tempfile.TemporaryDirectory() as td:
            parent = Path(td) / "Dev"
            src = parent / "f"
            dst = parent / "unearth"
            write_file(src / "a.txt", "a\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst)])
            self.assertEqual(rc, 0, out)
            self.assertIn("f/ (removed)", out)
            self.assertIn("unearth/", out)
            self.assertIn("Del(src)", out)

    def test_move_same_parent_rename_does_not_show_source_children_as_parent_siblings(self):
        with tempfile.TemporaryDirectory() as td:
            parent = Path(td) / "home"
            src = parent / "tasks"
            dst = parent / "ops"
            write_file(src / "laptop-alexandra" / "a.txt", "a\n")
            write_file(src / "local" / "b.txt", "b\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst)])
            self.assertEqual(rc, 0, out)
            self.assertIn("ops", out)
            self.assertIn("tasks/ (removed)", out)
            self.assertNotIn("\n├── laptop-alexandra/", out)
            self.assertNotIn("\n├── local/", out)
            self.assertNotIn("\n└── laptop-alexandra/", out)
            self.assertNotIn("\n└── local/", out)

    def test_move_same_filesystem_uses_fast_rename(self):
        with tempfile.TemporaryDirectory() as td:
            parent = Path(td) / "Dev"
            src = parent / "f"
            dst = parent / "unearth"
            write_file(src / "a.txt", "a\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Fast-path rename on same filesystem", out)
            self.assertFalse(src.exists(), out)
            self.assertTrue((dst / "a.txt").exists(), out)

    def test_move_empty_directory_rename_uses_fastpath_not_cleanup_only(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td) / "tasks"
            src = root / "alexandra"
            dst = root / "laptop-alexandra"
            src.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy(["--move", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Fast-path rename on same filesystem", out)
            self.assertNotIn("Starting move cleanup:", out)
            self.assertFalse(src.exists(), out)
            self.assertTrue(dst.exists(), out)

    def test_move_contents_only_to_new_named_target_uses_fast_rename(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td) / "Dev"
            src = root / "sites" / "swiftsay"
            dst = root / "swiftsay" / "swiftsay-server"
            write_file(src / "backend" / "api.txt", "x\n")
            (root / "swiftsay").mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy(["--move", str(src), str(dst), "-c"], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Fast-path rename on same filesystem", out)
            self.assertFalse(src.exists(), out)
            self.assertTrue((dst / "backend" / "api.txt").exists(), out)

    def test_source_star_behaves_like_contents_only(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src"
            dst = Path(td) / "dst"
            write_file(src / "sub" / "x.txt", "x\n")
            dst.mkdir(parents=True, exist_ok=True)
            rc_star, out_star, _ = run_copy(["--move", f"{src}/*", str(dst)])
            rc_c, out_c, _ = run_copy(["--move", f"{src}/", str(dst), "-c"])
            self.assertEqual(rc_star, 0)
            self.assertEqual(rc_c, 0)
            self.assertIn("Planned transfer bytes:", out_star)
            self.assertIn("Planned transfer bytes:", out_c)
            self.assertIn("Merge", out_star)
            self.assertIn("Merge", out_c)

    def test_move_source_star_prunes_all_empty_source_subdirs(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src"
            dst = Path(td) / "dst"
            write_file(src / "a" / "b" / "c" / "f.txt", "x\n")
            (src / "leftover" / "deep" / ".dthumb").mkdir(parents=True, exist_ok=True)
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy(["--move", f"{src}/*", str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            if src.exists():
                self.assertEqual(list(src.iterdir()), [], out)
            self.assertTrue((dst / "a" / "b" / "c" / "f.txt").exists(), out)

    def test_showall_abbreviation_format_present(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "change"
            dst = Path(td) / "dst"
            for i in range(20):
                write_file(src / f"n{i}.txt", f"{i}\n")
                write_file(dst / f"u{i}.txt", "u\n")
            rc, out, _ = run_copy(["-v", "-c", str(src), str(dst)])
            self.assertEqual(rc, 0)
            self.assertRegex(
                out,
                r"\.\.\. and (?:\d+ more (?:new|modified|identical|uncollided|deleted))(?: \d+ more (?:new|modified|identical|uncollided|deleted))*",
            )

    def test_showall_lists_identical_and_uncollided_with_expected_colors(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "same.txt", "same\n")
            write_file(src / "new.txt", "new\n")
            write_file(dst / "same.txt", "same\n")
            write_file(dst / "extra.txt", "extra\n")

            rc, out, raw = run_copy([str(src), str(dst), "-c", "--showall", "-L", "1"])
            self.assertEqual(rc, 0, out)
            self.assertIn("same.txt", out)
            self.assertIn("extra.txt", out)
            self.assertRegex(raw, r"\x1b\[96m[^\n]*same\.txt\x1b\[0m")
            self.assertRegex(raw, r"\x1b\[97m[^\n]*extra\.txt\x1b\[0m")

    def test_showall_fallback_orders_modified_new_identical_then_unchanged(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            for i in range(10):
                write_file(src / f"m{i:02d}.txt", "mod\n")
                write_file(dst / f"m{i:02d}.txt", "different-size\n")
                write_file(src / f"n{i:02d}.txt", "new\n")
                write_file(src / f"i{i:02d}.txt", "same\n")
                write_file(dst / f"i{i:02d}.txt", "same\n")
                write_file(dst / f"u{i:02d}.txt", "unchanged\n")

            rc, out, _ = run_copy([str(src), str(dst), "-c", "--showall", "-L", "1", "-T", "100"])
            self.assertEqual(rc, 0, out)
            rows = [
                line[4:]
                for line in out.splitlines()
                if line.startswith("├── ") or line.startswith("└── ")
            ]
            self.assertTrue(rows, out)

            first_m = next((idx for idx, name in enumerate(rows) if name.startswith("m")), None)
            first_n = next((idx for idx, name in enumerate(rows) if name.startswith("n")), None)
            first_i = next((idx for idx, name in enumerate(rows) if name.startswith("i")), None)
            first_u = next((idx for idx, name in enumerate(rows) if name.startswith("u")), None)

            self.assertIsNotNone(first_m, out)
            self.assertIsNotNone(first_n, out)
            self.assertIsNotNone(first_i, out)
            self.assertIsNotNone(first_u, out)
            self.assertLess(first_m, first_n, out)
            self.assertLess(first_n, first_i, out)
            self.assertLess(first_i, first_u, out)

    def test_non_verbose_top_level_truncates_to_25_with_summary(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            for i in range(30):
                write_file(src / f"f{i:02d}.txt", f"{i}\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy([str(src), str(dst), "-c"])
            self.assertEqual(rc, 0)
            tree_rows = [line for line in out.splitlines() if line.startswith("├── ") or line.startswith("└── ")]
            self.assertEqual(
                len(tree_rows),
                25,
                msg=f"expected 25 visible tree entries at default truncation, got {len(tree_rows)}\n{out}",
            )
            self.assertRegex(
                out,
                r"\.\.\. and (?:\d+ more (?:new|modified|identical|uncollided|deleted))(?: \d+ more (?:new|modified|identical|uncollided|deleted))*",
            )

    def test_non_showall_hides_identical_and_uncollided_entries(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "same.txt", "same\n")
            write_file(src / "new.txt", "new\n")
            write_file(dst / "same.txt", "same\n")
            write_file(dst / "extra.txt", "extra\n")

            rc, out, _ = run_copy([str(src), str(dst), "-c", "-L", "1"])
            self.assertEqual(rc, 0, out)
            self.assertIn("new.txt", out)
            self.assertNotIn("same.txt", out)
            self.assertNotIn("extra.txt", out)
            self.assertRegex(out, r"\.\.\. and .*more identical/uncollided")

    def test_non_verbose_auto_uses_showall_when_it_fits(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "folder" / "child.txt", "x\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy([str(src), str(dst), "-c"])
            self.assertEqual(rc, 0)
            # Auto mode should choose a hierarchical preview when it fits under the default line budget.
            self.assertIn("folder/", out)

    def test_default_depth_is_one_when_l_not_specified(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "l1" / "l2" / "leaf.txt", "x\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy([str(src), str(dst), "-c"])
            self.assertEqual(rc, 0, out)
            self.assertIn("l1/", out)
            self.assertNotIn("l2/", out)
            self.assertNotIn("leaf.txt", out)

    def test_tree_depth_flag_limits_visible_levels_exactly(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "l1" / "l2" / "leaf.txt", "x\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc1, out1, _ = run_copy([str(src), str(dst), "-c", "-L", "1"])
            self.assertEqual(rc1, 0, out1)
            self.assertIn("l1/", out1)
            self.assertNotIn("l2/", out1)
            self.assertNotIn("leaf.txt", out1)

            rc2, out2, _ = run_copy([str(src), str(dst), "-c", "-L", "2"])
            self.assertEqual(rc2, 0, out2)
            self.assertIn("l1/", out2)
            self.assertIn("l2/", out2)
            self.assertNotIn("leaf.txt", out2)

    def test_contents_only_uppercase_alias_rejected(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "f.txt", "x\n")
            dst.mkdir(parents=True)
            rc, out, _ = run_copy(["--move", "-C", str(src), str(dst)])
            self.assertNotEqual(rc, 0)
            self.assertIn("unrecognized arguments: -C", out)

    def test_verbose_alias_does_not_crash(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "f.txt", "x\n")
            dst.mkdir(parents=True)
            rc, out, _ = run_copy(["--move", "-v", str(src), str(dst)])
            self.assertEqual(rc, 0, out)
            self.assertIn("Planned transfer bytes:", out)

    def test_double_verbose_alias_is_rejected(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "f.txt", "x\n")
            dst.mkdir(parents=True)
            rc, out, _ = run_copy(["--move", "-vv", str(src), str(dst)])
            self.assertNotEqual(rc, 0)
            self.assertIn("unrecognized arguments: -vv", out)

    def test_verbose_keeps_default_tree_depth(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            for i in range(8):
                write_file(src / "newdir" / f"n{i}.txt", f"{i}\n")
            dst.mkdir(parents=True)
            rc, out, _ = run_copy(["-v", "-c", str(src), str(dst)])
            self.assertEqual(rc, 0, out)
            for i in range(8):
                self.assertNotIn(f"n{i}.txt", out, msg=out)

    def test_regular_files_summary_uses_new_modified_identical_uncollided(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "f.txt", "x\n")
            write_file(dst / "extra.txt", "z\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc_copy, out_copy, _ = run_copy([str(src), str(dst), "-c"])
            self.assertEqual(rc_copy, 0, out_copy)
            self.assertIn("Type", out_copy)
            self.assertIn("Del(src)", out_copy)
            self.assertRegex(out_copy, r"Files\s+\|\s*1\s+\|\s*0\s+\|\s*0\s+\|\s*1\s+\|\s*0\s+\|\s*0")

            rc_move, out_move, _ = run_copy(["--move", str(src), str(dst), "-c"])
            self.assertEqual(rc_move, 0, out_move)
            self.assertIn("Type", out_move)
            self.assertIn("Del(src)", out_move)
            self.assertRegex(out_move, r"Files\s+\|\s*1\s+\|\s*0\s+\|\s*0\s+\|\s*1\s+\|\s*1\s+\|\s*0")

    def test_uncollided_counts_destination_only_files_for_contents_merge_named_target(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "Android"
            dst = Path(td) / "dst" / "Android"
            write_file(src / "data" / "same.txt", "same\n")
            write_file(src / "data" / "new.txt", "new\n")
            write_file(dst / "data" / "same.txt", "same\n")
            write_file(dst / "other" / "keep1.txt", "k1\n")
            write_file(dst / "other" / "keep2.txt", "k2\n")

            rc, out, _ = run_copy([str(src), str(dst), "-c"])
            self.assertEqual(rc, 0, out)
            self.assertRegex(out, r"Files\s+\|\s*1\s+\|\s*0\s+\|\s*1\s+\|\s*2\s+\|\s*0\s+\|\s*0")

    def test_file_to_file_preview_counts_destination_sibling_as_uncollided(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "auth.json"
            dst_dir = Path(td) / "dst" / "accounts"
            dst = dst_dir / "personal2.json"
            write_file(src, "token\n")
            write_file(dst_dir / "other.json", "other\n")

            rc, out, _ = run_copy([str(src), str(dst)])
            self.assertEqual(rc, 0, out)
            self.assertRegex(out, r"Files\s+\|\s*1\s+\|\s*0\s+\|\s*0\s+\|\s*1\s+\|\s*0\s+\|\s*0")

    def test_file_rename_in_same_directory_does_not_count_source_as_uncollided(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td) / "content"
            src = root / "Nano.pmd"
            dst = root / "GNU_nano.pmd"
            write_file(src, "nano\n")
            write_file(root / "keep.pmd", "keep\n")

            rc, out, _ = run_copy(["--move", str(src), str(dst)])
            self.assertEqual(rc, 0, out)
            self.assertRegex(out, r"Files\s+\|\s*1\s+\|\s*0\s+\|\s*0\s+\|\s*1\s+\|\s*1\s+\|\s*0")

    def test_backup_merge_copy_creates_backup_dir(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst_root = Path(td) / "dst"
            dst = dst_root / "A"
            write_file(src / "new.txt", "new\n")
            write_file(dst / "old.txt", "old\n")

            rc, out, _ = run_copy(["-b", str(src), str(dst_root)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Backup saved as:", out)
            self.assertTrue((dst / "new.txt").exists())
            self.assertTrue((dst / "old.txt").exists())
            backups = find_backups(dst_root, "A")
            self.assertEqual(len(backups), 1, f"unexpected backups: {backups}")
            self.assertTrue((backups[0] / "old.txt").exists())

    def test_backup_merge_move_creates_backup_and_removes_source(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst_root = Path(td) / "dst"
            dst = dst_root / "A"
            write_file(src / "new.txt", "new\n")
            write_file(dst / "old.txt", "old\n")

            rc, out, _ = run_copy(["--move", "-b", str(src), str(dst_root)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Backup saved as:", out)
            self.assertFalse(src.exists())
            self.assertTrue((dst / "new.txt").exists())
            self.assertTrue((dst / "old.txt").exists())
            backups = find_backups(dst_root, "A")
            self.assertEqual(len(backups), 1, f"unexpected backups: {backups}")
            self.assertTrue((backups[0] / "old.txt").exists())

    def test_backup_overwrite_nested_target_move_replaces_and_backs_up_old(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "poo"
            dst_parent = Path(td) / "dst" / "root"
            dst = dst_parent / "poo"
            write_file(src / "new.txt", "new\n")
            write_file(dst / "old.txt", "old\n")

            rc, out, _ = run_copy(["--move", "-o", "-b", str(src), str(dst_parent)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Backup saved as:", out)
            self.assertTrue((dst / "new.txt").exists())
            self.assertFalse((dst / "old.txt").exists())
            backups = find_backups(dst_parent, "poo")
            self.assertEqual(len(backups), 1, f"unexpected backups: {backups}")
            self.assertTrue((backups[0] / "old.txt").exists())

    def test_backup_overwrite_explicit_contents_only_move_replaces_and_backs_up_old(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst_parent = Path(td) / "dst"
            dst = dst_parent / "B"
            write_file(src / "new.txt", "new\n")
            write_file(dst / "old.txt", "old\n")

            rc, out, _ = run_copy(["--move", "-o", "-c", "-b", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Backup saved as:", out)
            self.assertTrue((dst / "new.txt").exists())
            self.assertFalse((dst / "old.txt").exists())
            backups = find_backups(dst_parent, "B")
            self.assertEqual(len(backups), 1, f"unexpected backups: {backups}")
            self.assertTrue((backups[0] / "old.txt").exists())

    def test_backup_file_conflict_copy_creates_backup_file(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "f.txt"
            dst = Path(td) / "dst" / "f.txt"
            write_file(src, "newer\n")
            write_file(dst, "old\n")

            rc, out, _ = run_copy(["-b", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertIn("Backup saved as:", out)
            self.assertEqual(dst.read_text(encoding="utf-8"), "newer\n")
            backups = find_backups(dst.parent, "f.txt")
            self.assertEqual(len(backups), 1, f"unexpected backups: {backups}")
            self.assertTrue(backups[0].is_file())
            self.assertEqual(backups[0].read_text(encoding="utf-8"), "old\n")

    def test_backup_no_conflict_does_not_create_backup(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst"
            write_file(src / "n.txt", "n\n")
            dst.mkdir(parents=True, exist_ok=True)

            rc, out, _ = run_copy(["-b", str(src), str(dst)], confirm=True)
            self.assertEqual(rc, 0, out)
            self.assertNotIn("Backup complete.", out)
            self.assertTrue((dst / "A" / "n.txt").exists())
            backups = find_backups(dst, "A")
            self.assertEqual(len(backups), 0, f"unexpected backups: {backups}")

    def test_merge_collision_policy_dest_wins_keeps_destination_file(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst" / "A"
            write_file(src / "same.txt", "source-version\n")
            write_file(dst / "same.txt", "destination-version\n")

            rc, out, _ = run_copy(
                [str(src), str(dst), "-c", "--collision", "dest:always"],
                confirm=True,
            )
            self.assertEqual(rc, 0, out)
            self.assertEqual((dst / "same.txt").read_text(encoding="utf-8"), "destination-version\n")
            self.assertEqual((src / "same.txt").read_text(encoding="utf-8"), "source-version\n")
            self.assertIn("No changes detected; nothing to copy.", out)

    def test_merge_collision_policy_source_wins_replaces_destination_file(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst" / "A"
            write_file(src / "same.txt", "source-version\n")
            write_file(dst / "same.txt", "destination-version\n")

            rc, out, _ = run_copy(
                [str(src), str(dst), "-c", "--collision", "source:always"],
                confirm=True,
            )
            self.assertEqual(rc, 0, out)
            self.assertEqual((dst / "same.txt").read_text(encoding="utf-8"), "source-version\n")
            self.assertEqual((src / "same.txt").read_text(encoding="utf-8"), "source-version\n")

    def test_merge_collision_policy_source_wins_if_larger_only_replaces_when_source_larger(self):
        with tempfile.TemporaryDirectory() as td:
            src_larger = Path(td) / "src-larger" / "A"
            dst_larger = Path(td) / "dst-larger" / "A"
            write_file(src_larger / "same.txt", "source-is-larger\n")
            write_file(dst_larger / "same.txt", "dst\n")

            rc, out, _ = run_copy(
                [str(src_larger), str(dst_larger), "-c", "--collision", "source:larger"],
                confirm=True,
            )
            self.assertEqual(rc, 0, out)
            self.assertEqual((dst_larger / "same.txt").read_text(encoding="utf-8"), "source-is-larger\n")

        with tempfile.TemporaryDirectory() as td:
            src_smaller = Path(td) / "src-smaller" / "A"
            dst_smaller = Path(td) / "dst-smaller" / "A"
            write_file(src_smaller / "same.txt", "src\n")
            write_file(dst_smaller / "same.txt", "destination-is-larger\n")

            rc, out, _ = run_copy(
                [str(src_smaller), str(dst_smaller), "-c", "--collision", "source:larger"],
                confirm=True,
            )
            self.assertEqual(rc, 0, out)
            self.assertEqual((dst_smaller / "same.txt").read_text(encoding="utf-8"), "destination-is-larger\n")

    def test_merge_collision_policy_source_wins_if_newer_or_larger_replaces_same_size_newer_source(self):
        with tempfile.TemporaryDirectory() as td:
            src = Path(td) / "src" / "A"
            dst = Path(td) / "dst" / "A"
            write_file(src / "same.txt", "source1\n")
            write_file(dst / "same.txt", "dest__1_\n")
            dst_file = dst / "same.txt"
            src_file = src / "same.txt"
            older = 1_650_000_000
            newer = older + 100
            os.utime(dst_file, (older, older))
            os.utime(src_file, (newer, newer))

            rc, out, _ = run_copy(
                [str(src), str(dst), "-c", "--collision", "source:newer,larger"],
                confirm=True,
            )
            self.assertEqual(rc, 0, out)
            self.assertEqual(dst_file.read_text(encoding="utf-8"), "source1\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
