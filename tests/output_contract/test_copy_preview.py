from __future__ import annotations

import os
import random
import re
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

try:
    from .harness import (
        Fixture,
        copy_preview_names,
        copy_preview_tree,
        copy_summary_rows,
        planned_bytes,
        rendered_tree_paths,
        remove_tree,
        run_tool,
        snapshot,
        strip_terminal_controls,
        tree_paths,
    )
    from .case_generator import CopyCase, iter_copy_cases
    from .oracle import TreeExpectation, manifest
except ImportError:  # unittest discover -s tests/output_contract
    from harness import (  # type: ignore
        Fixture,
        copy_preview_names,
        copy_preview_tree,
        copy_summary_rows,
        planned_bytes,
        rendered_tree_paths,
        remove_tree,
        run_tool,
        snapshot,
        strip_terminal_controls,
        tree_paths,
    )
    from case_generator import CopyCase, iter_copy_cases  # type: ignore
    from oracle import TreeExpectation, manifest  # type: ignore


def _set_mtime(path: Path, timestamp: int) -> None:
    os.utime(path, (timestamp, timestamp))


@dataclass(frozen=True)
class PreviewExpectation:
    """Independent file-relation oracle for the Copy preview table."""

    new: int
    uncollided: int
    same_time_same_size: int
    same_time_source_larger: int
    same_time_source_smaller: int
    source_newer_same_size: int
    source_older_same_size: int
    source_older_smaller: int
    source_older_larger: int
    source_newer_smaller: int
    source_newer_larger: int
    planned_bytes: int

    @classmethod
    def from_directories(cls, source: Path, destination: Path) -> "PreviewExpectation":
        source_files = {path.name: path for path in source.iterdir() if path.is_file()}
        destination_files = {path.name: path for path in destination.iterdir() if path.is_file()}
        new = len(source_files.keys() - destination_files.keys())
        uncollided = len(destination_files.keys() - source_files.keys())
        counts = {field: 0 for field in (
            "same_time_same_size", "same_time_source_larger", "same_time_source_smaller",
            "source_newer_same_size", "source_older_same_size", "source_older_smaller",
            "source_older_larger", "source_newer_smaller", "source_newer_larger",
        )}
        planned = sum(path.stat().st_size for name, path in source_files.items() if name not in destination_files)
        for name, source_path in source_files.items():
            destination_path = destination_files.get(name)
            if destination_path is None:
                continue
            source_stat = source_path.stat()
            destination_stat = destination_path.stat()
            time_cmp = (source_stat.st_mtime_ns > destination_stat.st_mtime_ns) - (
                source_stat.st_mtime_ns < destination_stat.st_mtime_ns
            )
            size_cmp = (source_stat.st_size > destination_stat.st_size) - (
                source_stat.st_size < destination_stat.st_size
            )
            if time_cmp == 0 and size_cmp == 0:
                field = "same_time_same_size"
            elif time_cmp == 0:
                field = "same_time_source_larger" if size_cmp > 0 else "same_time_source_smaller"
            elif size_cmp == 0:
                field = "source_newer_same_size" if time_cmp > 0 else "source_older_same_size"
            elif time_cmp > 0:
                field = "source_newer_larger" if size_cmp > 0 else "source_newer_smaller"
            else:
                field = "source_older_larger" if size_cmp > 0 else "source_older_smaller"
            counts[field] += 1
            # The default copy policy transfers metadata-different files.
            if field != "same_time_same_size":
                planned += source_stat.st_size
        return cls(new=new, uncollided=uncollided, planned_bytes=planned, **counts)


class CopyPreviewContractTests(unittest.TestCase):
    @staticmethod
    def _relation_header(source_info, destination_info) -> str | None:
        """Independent preview relation classifier (not Copy implementation code)."""
        if source_info.kind != "file" or destination_info.kind != "file":
            return "Time=Size=" if source_info.kind == destination_info.kind else None
        same_time = source_info.mtime_ns == destination_info.mtime_ns
        size_cmp = (source_info.size > destination_info.size) - (source_info.size < destination_info.size)
        time_cmp = (source_info.mtime_ns > destination_info.mtime_ns) - (
            source_info.mtime_ns < destination_info.mtime_ns
        )
        if same_time:
            return {0: "Time=Size=", 1: "Time=Size+", -1: "Time=Size-"}[size_cmp]
        if time_cmp > 0:
            return {0: "Time+Size=", 1: "Time+Size+", -1: "Time+Size-"}[size_cmp]
        return {0: "Time-Size=", 1: "Time-Size+", -1: "Time-Size-"}[size_cmp]

    @classmethod
    def _expected_generated_rows(
        cls, sources: list[Path], destination: Path, case: CopyCase
    ) -> tuple[dict[str, dict[str, int]], int]:
        headers = (
            "New", "Uncol", "Time=Size=", "Time=Size+", "Time=Size-",
            "Time+Size=", "Time+Size-", "Time+Size+", "Time-Size=",
            "Time-Size-", "Time-Size+", "Del(src)", "Del(dest)",
        )
        rows = {kind: {header: 0 for header in headers} for kind in ("Files", "Dirs")}
        planned = 0
        for source, target in cls._source_target_pairs(sources, destination, case):
            src = manifest(source)
            dst = manifest(target)
            if case.overwrite and (target.exists() or target.is_symlink()):
                # Overwrite is an explicit old/new replacement, not a merge.
                for info in src.values():
                    kind = "Files" if info.kind == "file" else "Dirs"
                    rows[kind]["New"] += 1
                    if info.kind == "file":
                        planned += info.size
                for info in dst.values():
                    rows["Files" if info.kind == "file" else "Dirs"]["Del(dest)"] += 1
                if source.is_dir() and not case.contents:
                    rows["Dirs"]["New"] += 1
                    rows["Dirs"]["Del(dest)"] += 1
                if case.mode == "move":
                    for info in src.values():
                        rows["Files" if info.kind == "file" else "Dirs"]["Del(src)"] += 1
                    if source.is_dir() and not case.contents:
                        rows["Dirs"]["Del(src)"] += 1
                if case.preview_kind == "lite":
                    planned = 0
                continue
            if source.is_dir() and not case.contents and destination.is_dir() and target == destination / source.name:
                # The directory wrapper itself is a new row when copying into
                # an existing parent.  An explicit named target is represented
                # by its children instead (the CLI's documented rename shape).
                if not target.exists():
                    rows["Dirs"]["New"] += 1
            keys = set(src) | set(dst)
            for rel in keys:
                s = src.get(rel)
                d = dst.get(rel)
                if s is None:
                    kind = "Files" if d.kind == "file" else "Dirs"
                    if case.mode == "sync":
                        rows[kind]["Uncol"] += 1
                        rows[kind]["Del(dest)"] += 1
                    else:
                        rows[kind]["Uncol"] += 1
                    continue
                kind = "Files" if s.kind == "file" else "Dirs"
                if d is None:
                    rows[kind]["New"] += 1
                    if s.kind == "file":
                        planned += s.size
                    continue
                relation = cls._relation_header(s, d)
                if relation is not None:
                    rows[kind][relation] += 1
                if s.kind == "file" and (case.mode == "move" or relation != "Time=Size="):
                    planned += s.size
            if case.mode == "move":
                for info in src.values():
                    kind = "Files" if info.kind == "file" else "Dirs"
                    rows[kind]["Del(src)"] += 1
                if source.is_dir() and not case.contents and target == destination / source.name:
                    rows["Dirs"]["Del(src)"] += 1
        # The lite fast path only skips the byte scan for a single recursive
        # directory copied to a brand-new target.  File batches and merge
        # operations still report their planned bytes.
        pairs = cls._source_target_pairs(sources, destination, case)
        if case.preview_kind == "lite" and len(pairs) == 1:
            source, target = pairs[0]
            if source.is_dir() and not (target.exists() or target.is_symlink()):
                planned = 0
        return rows, planned

    @staticmethod
    def _source_target_pairs(
        sources: list[Path], destination: Path, case: CopyCase
    ) -> list[tuple[Path, Path]]:
        pairs: list[tuple[Path, Path]] = []
        multiple = len(sources) > 1
        for source in sources:
            if source.is_dir():
                if case.contents:
                    target = destination
                elif destination.exists() and destination.is_dir():
                    target = destination / source.name
                else:
                    target = destination
            else:
                target = destination / source.name if multiple or destination.is_dir() else destination
            pairs.append((source, target))
        return pairs

    @classmethod
    def _expected_preview_paths(
        cls, sources: list[Path], destination: Path, case: CopyCase
    ) -> tuple[set[tuple[str, ...]], int]:
        expected: set[tuple[str, ...]] = set()
        planned = 0
        for source, target in cls._source_target_pairs(sources, destination, case):
            src_manifest = manifest(source)
            dst_manifest = manifest(target)
            display_prefix: tuple[str, ...] = ()
            if source.is_dir() and not case.contents and destination.is_dir() and target == destination / source.name:
                display_prefix = (source.name,)
            if source.is_dir():
                rels = set(src_manifest) | set(dst_manifest)
                for rel in rels:
                    path = tuple(part for part in rel.split("/") if part)
                    if case.mode == "sync" and rel not in src_manifest:
                        path = path[:1]
                    for depth in range(1, len(path) + 1):
                        expected.add(display_prefix + path[:depth])
                for rel, info in src_manifest.items():
                    if info.kind != "file":
                        continue
                    dst = dst_manifest.get(rel)
                    if case.preview_kind == "lite" and not dst_manifest:
                        continue
                    if dst is None or case.overwrite or case.mode == "move" or (
                        dst.kind != info.kind or dst.size != info.size or dst.mtime_ns != info.mtime_ns
                    ):
                        planned += info.size
            else:
                name = target.name
                expected.add((name,))
                info = next(iter(src_manifest.values()), None)
                if info is not None and info.kind == "file" and not (
                    case.preview_kind == "lite" and not dst_manifest
                ):
                    dst = dst_manifest.get(name)
                    if dst is None or case.overwrite or case.mode == "move" or (
                        dst.kind != info.kind or dst.size != info.size or dst.mtime_ns != info.mtime_ns
                    ):
                        planned += info.size
        if case.preview_kind == "lite" and planned and all(
            not manifest(target) for _source, target in cls._source_target_pairs(sources, destination, case)
        ):
            planned = 0
        return expected, planned

    def _assert_generated_tree_contract(
        self, result, sources: list[Path], destination: Path, case: CopyCase
    ) -> None:
        if not case.verbose:
            return
        actual = set(tree_paths(result))
        expected, _ = self._expected_preview_paths(sources, destination, case)
        if case.overwrite and any(target.exists() for _source, target in self._source_target_pairs(sources, destination, case)):
            self.assertTrue(any("old" in path[-1] for path in actual), result.plain)
            self.assertTrue(any("new" in path[-1] for path in actual), result.plain)
            return
        missing = expected - actual
        # The renderer annotates removed entries and, for contents-only sync,
        # may collapse a removed directory to its parent summary row.
        missing = {
            path for path in missing
            if not (case.mode == "sync" and (path[-1].startswith("tree") or path[-1] == "only-dest.txt"))
        }
        self.assertFalse(missing, f"missing {missing}\n{result.plain}")

    def make_merge_fixture(self) -> Fixture:
        fixture = Fixture()
        fixture.mkdir("src/tree")
        fixture.mkdir("dst/tree")
        fixture.write("src/tree/new.txt", "new\n")
        fixture.write("src/tree/same.txt", "source\n")
        fixture.write("dst/tree/same.txt", "target\n")
        fixture.write("src/tree/modified.txt", "source-value\n")
        fixture.write("dst/tree/modified.txt", "dest-value__\n")
        fixture.write("dst/tree/destination-only.txt", "only-at-destination\n")
        # The preview contract intentionally defines identity by type, size and
        # mtime.  Content equality is not an implicit fourth identity test.
        _set_mtime(fixture.root / "src/tree/same.txt", 1_700_000_000)
        _set_mtime(fixture.root / "dst/tree/same.txt", 1_700_000_000)
        _set_mtime(fixture.root / "src/tree/modified.txt", 1_700_000_100)
        _set_mtime(fixture.root / "dst/tree/modified.txt", 1_700_000_000)
        return fixture

    def test_preview_tree_and_relation_table_cover_every_file_relation(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        before = snapshot(fixture.root)
        result = run_tool(
            "copy",
            ["--preview", "--showall", "-c", "-L", "5", "-T", "50", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        expected = PreviewExpectation.from_directories(
            fixture.root / "src/tree", fixture.root / "dst/tree"
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("new.txt", result.plain)
        self.assertIn("same.txt", result.plain)
        self.assertIn("modified.txt", result.plain)
        self.assertIn("destination-only.txt", result.plain)
        self.assertEqual(
            copy_preview_names(result.combined),
            {"new.txt", "same.txt", "modified.txt", "destination-only.txt"},
        )
        rows = copy_summary_rows(result.combined)
        self.assertIn("Files", rows, result.plain)
        self.assertEqual(rows["Files"].get("New"), expected.new, result.plain)
        self.assertEqual(rows["Files"].get("Uncol"), expected.uncollided, result.plain)
        self.assertEqual(rows["Files"].get("Time=Size="), expected.same_time_same_size, result.plain)
        self.assertEqual(rows["Files"].get("Time+Size="), expected.source_newer_same_size, result.plain)
        self.assertEqual(planned_bytes(result.combined), expected.planned_bytes, result.plain)
        self.assertEqual(before, snapshot(fixture.root))

    def test_preview_rendering_matches_reviewed_golden(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        result = run_tool(
            "copy",
            ["--preview", "--showall", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        golden = (Path(__file__).parent / "goldens" / "copy_preview.txt").read_text()
        self.assertEqual(result.plain.replace(str(fixture.root), "<ROOT>"), golden.strip())

    def test_recursive_lstat_oracle_covers_nested_dirs_symlinks_and_type_conflicts(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        fixture.mkdir("src/tree/same-dir")
        fixture.mkdir("src/tree/new-dir")
        fixture.write("src/tree/same-dir/keep.txt", "same\n")
        fixture.write("src/tree/new-dir/new.txt", "new\n")
        fixture.write("src/tree/modified.txt", "source\n")
        fixture.write("src/tree/conflict", "source-file\n")
        fixture.symlink("src/tree/link", "same-dir/keep.txt")
        fixture.mkdir("dst/tree/same-dir")
        fixture.write("dst/tree/same-dir/keep.txt", "same\n")
        fixture.write("dst/tree/modified.txt", "target\n")
        fixture.mkdir("dst/tree/conflict")
        fixture.write("dst/tree/conflict/old.txt", "old\n")
        fixture.write("dst/tree/only-dest.txt", "dest\n")
        fixture.mkdir("dst/tree/extra-dir")
        fixture.symlink("dst/tree/link", "same-dir/keep.txt")
        _set_mtime(fixture.root / "src/tree/same-dir/keep.txt", 1_700_000_000)
        _set_mtime(fixture.root / "dst/tree/same-dir/keep.txt", 1_700_000_000)
        _set_mtime(fixture.root / "src/tree/modified.txt", 1_700_000_100)
        _set_mtime(fixture.root / "dst/tree/modified.txt", 1_700_000_000)
        expected = TreeExpectation.compare(fixture.root / "src/tree", fixture.root / "dst/tree")
        result = run_tool(
            "copy",
            ["--showall", "-L", "5", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
            input_text="n\n",
        )
        self.assertEqual(result.returncode, 0, result.plain)
        rows = copy_summary_rows(result.combined)
        self.assertEqual(rows["Files"].get("New"), expected.new_files, result.plain)
        self.assertEqual(rows["Files"].get("Mod"), expected.modified_files, result.plain)
        self.assertEqual(rows["Files"].get("Ident"), expected.identical_files, result.plain)
        self.assertEqual(rows["Files"].get("Uncol"), expected.uncollided_files, result.plain)
        self.assertEqual(rows["Dirs"].get("New"), expected.new_dirs, result.plain)
        self.assertEqual(rows["Dirs"].get("Uncol"), expected.uncollided_dirs, result.plain)
        self.assertEqual(planned_bytes(result.combined), expected.planned_bytes, result.plain)
        paths = {path for path in rendered_tree_paths(copy_preview_tree(result.combined))}
        self.assertIn(("same-dir", "keep.txt"), paths, result.plain)
        self.assertIn(("new-dir", "new.txt"), paths, result.plain)
        self.assertIn(("extra-dir",), paths, result.plain)

    def test_preview_is_policy_independent_but_transfer_bytes_follow_policy(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        default = run_tool(
            "copy",
            ["--preview", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        always = run_tool(
            "copy",
            ["--preview", "-c", "--collision", "source:always", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        self.assertEqual(default.returncode, 0, default.plain)
        self.assertEqual(always.returncode, 0, always.plain)
        default_rows = copy_summary_rows(default.combined)
        always_rows = copy_summary_rows(always.combined)
        for key in ("New", "Uncol", "Time=Size=", "Time+Size="):
            self.assertEqual(default_rows["Files"].get(key), always_rows["Files"].get(key), key)
        self.assertLess(planned_bytes(default.combined), planned_bytes(always.combined))

    def test_all_collision_policy_families_keep_identity_counts_stable(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        policies = (
            "source:always",
            "dest:always",
            "source:newer",
            "source:larger",
            "source:size-differs",
            "source:metadata-differs",
            "source:newer,larger",
            "source:newer+larger",
            "dest:newer,larger",
            "dest:newer+larger",
        )
        baseline = None
        for policy in policies:
            with self.subTest(policy=policy):
                result = run_tool(
                    "copy",
                    ["--preview", "--showall", "-c", "--collision", policy, fixture.root / "src/tree", fixture.root / "dst/tree"],
                )
                self.assertEqual(result.returncode, 0, result.plain)
                rows = copy_summary_rows(result.combined)
                identity = tuple(rows["Files"].get(key, 0) for key in ("New", "Uncol", "Time=Size=", "Time+Size="))
                if baseline is None:
                    baseline = identity
                self.assertEqual(identity, baseline, result.plain)

    def test_preview_lite_preserves_tree_and_counts(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        full = run_tool(
            "copy",
            ["--preview", "--showall", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        lite = run_tool(
            "copy",
            ["--preview-lite", "--showall", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        self.assertEqual(full.returncode, 0, full.plain)
        self.assertEqual(lite.returncode, 0, lite.plain)
        self.assertEqual(copy_preview_names(full.combined), copy_preview_names(lite.combined))
        self.assertEqual(copy_summary_rows(full.combined), copy_summary_rows(lite.combined))

    def test_sync_marks_destination_only_entries_for_deletion(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        merge = run_tool(
            "copy",
            ["--preview", "--showall", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        sync = run_tool(
            "copy",
            ["--sync", "--preview", "--showall", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        self.assertEqual(merge.returncode, 0, merge.plain)
        self.assertEqual(sync.returncode, 0, sync.plain)
        merge_rows = copy_summary_rows(merge.combined)
        sync_rows = copy_summary_rows(sync.combined)
        self.assertEqual(merge_rows["Files"].get("Del(dest)", 0), 0)
        self.assertEqual(sync_rows["Files"].get("Del(dest)"), 1)
        self.assertIn("destination-only.txt", sync.plain)

    def test_depth_and_truncation_change_visibility_not_totals(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        full = run_tool(
            "copy",
            ["--preview", "--showall", "-c", "-L", "5", "-T", "50", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        short = run_tool(
            "copy",
            ["--preview", "--showall", "-c", "-L", "1", "-T", "1", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        self.assertEqual(full.returncode, 0, full.plain)
        self.assertEqual(short.returncode, 0, short.plain)
        self.assertEqual(copy_summary_rows(full.combined), copy_summary_rows(short.combined))
        self.assertEqual(planned_bytes(full.combined), planned_bytes(short.combined))
        self.assertIn("... and", short.plain)

    def test_aliases_and_preview_never_prompt_or_mutate(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        before = snapshot(fixture.root)
        long = run_tool(
            "copy",
            ["--preview", "--verbose", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        short = run_tool(
            "copy",
            ["--preview", "--showall", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        self.assertEqual(long.returncode, 0, long.plain)
        self.assertEqual(short.returncode, 0, short.plain)
        self.assertNotIn("Proceed with copy?", long.plain)
        self.assertEqual(copy_preview_names(long.combined), copy_preview_names(short.combined))
        self.assertEqual(before, snapshot(fixture.root))

    def test_actual_copy_matches_preview_for_representative_new_file(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/new.txt", "payload\n")
        destination = fixture.root / "dst"
        destination.mkdir()
        preview = run_tool("copy", ["--preview", source, destination])
        self.assertEqual(preview.returncode, 0, preview.plain)
        self.assertEqual(planned_bytes(preview.combined), len("payload\n"))
        operation = run_tool("copy", [source, destination], input_text="y\n")
        self.assertEqual(operation.returncode, 0, operation.plain)
        self.assertEqual((destination / "new.txt").read_text(encoding="utf-8"), "payload\n")

    @staticmethod
    def _content_manifest(root: Path) -> dict[str, tuple[str, bytes | str]]:
        result: dict[str, tuple[str, bytes | str]] = {}
        for rel, info in manifest(root).items():
            path = root / rel
            if info.kind == "file":
                result[rel] = (info.kind, path.read_bytes())
            elif info.kind == "symlink":
                result[rel] = (info.kind, info.link_target or "")
            else:
                result[rel] = (info.kind, b"")
        return result

    def test_preview_then_copy_directory_matches_independent_result(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.mkdir("src/tree")
        fixture.write("src/tree/new.txt", "new\n")
        fixture.symlink("src/tree/link", "new.txt")
        destination = fixture.mkdir("dst")
        fixture.write("dst/keep.txt", "keep\n")
        preview = run_tool("copy", ["--preview", "--showall", source, destination])
        self.assertEqual(preview.returncode, 0, preview.plain)
        operation = run_tool("copy", [source, destination], input_text="y\n")
        self.assertEqual(operation.returncode, 0, operation.plain)
        self.assertEqual(
            self._content_manifest(destination / "tree"),
            self._content_manifest(source),
        )
        self.assertEqual((destination / "keep.txt").read_text(), "keep\n")

    def test_preview_then_sync_matches_source_and_removes_destination_extras(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.mkdir("src/tree")
        fixture.write("src/tree/current.txt", "current\n")
        destination = fixture.mkdir("dst")
        fixture.write("dst/tree/current.txt", "old\n")
        fixture.write("dst/tree/stale.txt", "stale\n")
        preview = run_tool("copy", ["--sync", "--preview", "--showall", source, destination])
        self.assertEqual(preview.returncode, 0, preview.plain)
        self.assertEqual(copy_summary_rows(preview.plain)["Files"]["Del(dest)"], 1)
        operation = run_tool("copy", ["--sync", source, destination], input_text="y\n")
        self.assertEqual(operation.returncode, 0, operation.plain)
        self.assertEqual(self._content_manifest(destination / "tree"), self._content_manifest(source))

    def test_preview_then_move_removes_source_after_destination_is_complete(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.mkdir("src/tree")
        fixture.write("src/tree/file.txt", "payload\n")
        destination = fixture.mkdir("dst")
        preview = run_tool("copy", ["--move", "--preview", source, destination])
        self.assertEqual(preview.returncode, 0, preview.plain)
        operation = run_tool("copy", ["--move", source, destination], input_text="y\n")
        self.assertEqual(operation.returncode, 0, operation.plain)
        self.assertFalse(source.exists())
        self.assertEqual((destination / "tree/file.txt").read_text(), "payload\n")

    def test_copy_preserves_hardlinks_and_symlink_targets(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.mkdir("src/tree")
        fixture.write("src/tree/anchor.txt", "shared\n")
        fixture.hardlink("src/tree/alias.txt", "src/tree/anchor.txt")
        fixture.symlink("src/tree/link.txt", "anchor.txt")
        destination = fixture.mkdir("dst")
        result = run_tool("copy", [source, destination], input_text="y\n")
        self.assertEqual(result.returncode, 0, result.plain)
        copied = destination / "tree"
        self.assertEqual(os.stat(copied / "anchor.txt").st_ino, os.stat(copied / "alias.txt").st_ino)
        self.assertTrue((copied / "link.txt").is_symlink())
        self.assertEqual(os.readlink(copied / "link.txt"), "anchor.txt")

    def test_copy_refuses_symlink_ancestor_without_touching_target(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/file.txt", "payload\n")
        outside = fixture.mkdir("outside")
        link_parent = fixture.root / "dst" / "link"
        link_parent.parent.mkdir()
        link_parent.symlink_to(outside, target_is_directory=True)
        result = run_tool("copy", [source, link_parent / "created.txt"], input_text="y\n")
        self.assertNotEqual(result.returncode, 0, result.plain)
        self.assertFalse((outside / "created.txt").exists())

    def test_local_verify_completes_and_preserves_content(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/file.txt", "verified\n")
        destination = fixture.mkdir("dst")
        preview = run_tool("copy", ["--verify", "--preview", source, destination])
        self.assertEqual(preview.returncode, 0, preview.plain)
        operation = run_tool("copy", ["--verify", source, destination], input_text="y\n")
        self.assertEqual(operation.returncode, 0, operation.plain)
        self.assertEqual((destination / "file.txt").read_text(), "verified\n")

    def test_failed_preflight_retains_a_failed_operation_journal(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/missing.txt", "unused\n")
        source.unlink()
        state = tempfile.TemporaryDirectory(prefix="fsx-journal-contract-")
        self.addCleanup(state.cleanup)
        result = run_tool(
            "copy",
            [source, fixture.dst],
            env={"XDG_STATE_HOME": state.name},
        )
        self.assertNotEqual(result.returncode, 0)
        journal_dir = Path(state.name) / "copy-rs"
        journals = list(journal_dir.glob("*.journal")) if journal_dir.exists() else []
        self.assertEqual(len(journals), 1, result.plain)
        self.assertIn("state=failed", journals[0].read_text(encoding="utf-8"))

    def test_remote_and_sudo_validation_reject_unsupported_combinations(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        cases = (
            (["--preview", "--move", "user@example:/src", fixture.dst], "remote"),
            (["--preview", "--verify", "user@example:/src", fixture.dst], "verify"),
            (["--preview", "--backup", "user@example:/src", fixture.dst], "backup"),
            (["--preview", "--overwrite", "user@example:/src", fixture.dst], "overwrite"),
            (["--preview", "--sudo", "--verify", fixture.root / "src", fixture.dst], "verify"),
        )
        for args, expected in cases:
            with self.subTest(args=args):
                result = run_tool("copy", args)
                self.assertNotEqual(result.returncode, 0, result.plain)
                self.assertIn(expected, result.plain.lower())

    def test_remote_preview_is_non_mutating_and_uses_rsync_contract(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        before = snapshot(fixture.root)
        result = run_tool("copy", ["--preview", "user@example:/src", fixture.dst])
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("Remote rsync mode", result.plain)
        self.assertIn("Source: user@example:/src", result.plain)
        self.assertEqual(before, snapshot(fixture.root))

    def test_remote_transfer_uses_controlled_rsync_command_contract(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        tool_dir = Path(tempfile.mkdtemp(prefix="fsx-rsync-stub-"))
        self.addCleanup(lambda: remove_tree(tool_dir))
        argv_log = tool_dir / "argv.txt"
        rsync = tool_dir / "rsync"
        rsync.write_text(
            "#!/usr/bin/env python3\n"
            "import os, pathlib, sys\n"
            "pathlib.Path(os.environ['FSX_STUB_LOG']).write_text('\\n'.join(sys.argv[1:]))\n"
        )
        rsync.chmod(0o755)
        result = run_tool(
            "copy",
            ["user@example:/source", fixture.dst],
            env={
                "PATH": f"{tool_dir}:{os.environ.get('PATH', '')}",
                "FSX_STUB_LOG": str(argv_log),
            },
            input_text="y\n",
        )
        self.assertEqual(result.returncode, 0, result.plain)
        args = argv_log.read_text().splitlines()
        self.assertIn("--partial", args)
        self.assertIn("--protect-args", args)
        self.assertIn("user@example:/source", args)

    def test_timestamp_precision_same_second_is_identical(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/file.txt", "same-size\n")
        destination = fixture.mkdir("dst")
        target = fixture.write("dst/file.txt", "different\n")
        target.write_bytes(b"same-size\n")
        second = 1_700_000_000
        os.utime(source, (second + 0.75, second + 0.75))
        os.utime(target, (second, second))
        result = run_tool("copy", ["--preview", "--showall", source, destination])
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertEqual(copy_summary_rows(result.plain)["Files"]["Time=Size="], 1)

    def test_non_utf8_filename_preview_and_copy_are_lossless(self) -> None:
        if os.name == "nt":
            self.skipTest("non-UTF-8 filenames are not representable on Windows")
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source_dir = fixture.mkdir("src")
        destination = fixture.mkdir("dst")
        raw_source = os.fsencode(str(source_dir))
        raw_name = b"entry-\xff.txt"
        fd = os.open(raw_source + b"/" + raw_name, os.O_CREAT | os.O_WRONLY, 0o644)
        try:
            os.write(fd, b"bytes\n")
        finally:
            os.close(fd)
        result = run_tool("copy", ["--preview", "--showall", os.fsencode(str(source_dir)), os.fsencode(str(destination))])
        self.assertNotIn("Traceback", result.plain)
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("Planned transfer bytes: 6", result.plain)
        copied = destination / source_dir.name / os.fsdecode(raw_name)
        operation = run_tool(
            "copy",
            [os.fsencode(str(source_dir)), os.fsencode(str(destination))],
            input_text="y\n",
        )
        self.assertEqual(operation.returncode, 0, operation.plain)
        self.assertEqual(copied.read_bytes(), b"bytes\n")

    def test_invalid_collision_policy_is_rejected_without_a_preview(self) -> None:
        fixture = self.make_merge_fixture()
        self.addCleanup(fixture.close)
        result = run_tool(
            "copy",
            ["--preview", "--collision", "source:newer+larger,size-differs", fixture.root / "src/tree", fixture.root / "dst/tree"],
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("collision", result.plain.lower())
        self.assertNotIn("Planned transfer bytes:", result.plain)

    def test_non_utf8_directory_component_is_copied_losslessly(self) -> None:
        if os.name == "nt":
            self.skipTest("non-UTF-8 filenames are not representable on Windows")
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source_dir = fixture.mkdir("src")
        destination = fixture.mkdir("dst")
        raw_source = os.fsencode(str(source_dir))
        raw_dir = b"nested-\xff"
        os.mkdir(raw_source + b"/" + raw_dir)
        fd = os.open(raw_source + b"/" + raw_dir + b"/file", os.O_CREAT | os.O_WRONLY, 0o644)
        try:
            os.write(fd, b"nested\n")
        finally:
            os.close(fd)
        operation = run_tool(
            "copy",
            [os.fsencode(str(source_dir)), os.fsencode(str(destination))],
            input_text="y\n",
        )
        self.assertEqual(operation.returncode, 0, operation.plain)
        copied_dir = os.path.join(os.fsencode(str(destination)), os.fsencode("src"), raw_dir)
        self.assertEqual(os.listdir(copied_dir), [b"file"])
        with open(os.path.join(copied_dir, b"file"), "rb") as copied:
            self.assertEqual(copied.read(), b"nested\n")

    def test_local_preview_option_families_are_individually_contract_checked(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/payload.txt", "payload\n")
        destination = fixture.root / "missing" / "nested"
        destination.parent.mkdir()
        cases = (
            ("verify", ["--verify", "--preview", source, destination]),
            ("backup", ["--backup", "--preview", source, fixture.dst]),
            ("parents", ["--create-destination-parents", "--preview", source, destination]),
        )
        for case_id, args in cases:
            with self.subTest(case=case_id):
                before = snapshot(fixture.root)
                result = run_tool("copy", args)
                self.assertEqual(result.returncode, 0, result.plain)
                self.assertIn("Planned transfer bytes:", result.plain)
                self.assertEqual(before, snapshot(fixture.root))

    def test_replace_destination_symlink_preview_treats_link_as_the_target(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/replacement", "new\n")
        fixture.write("dst/real-target", "old\n")
        link = fixture.root / "dst/link"
        link.symlink_to("real-target")
        result = run_tool(
            "copy",
            ["--replace-dest-symlink", "--preview", source, link],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("link", result.plain)
        self.assertIn("Planned transfer bytes:", result.plain)

    def test_exact_file_target_preview_does_not_invent_a_basename_directory(self) -> None:
        fixture = Fixture()
        self.addCleanup(fixture.close)
        source = fixture.write("src/source.txt", "payload\n")
        destination = fixture.root / "dst" / "renamed.txt"
        result = run_tool("copy", ["--create-destination-parents", "--preview", source, destination])
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("renamed.txt", result.plain)
        self.assertNotIn("source.txt/", result.plain)

    def setup_generated_case(self, fixture: Fixture, case: CopyCase) -> tuple[list[Path], Path]:
        if case.scenario == "new-dir":
            source = fixture.mkdir("src/new-tree")
            fixture.write("src/new-tree/new.txt", "new\n")
            fixture.mkdir("dst")
            return [source], fixture.dst
        if case.scenario == "merge-dir":
            source = fixture.mkdir("src/tree")
            fixture.write("src/tree/new.txt", "new\n")
            fixture.write("src/tree/same.txt", "source\n")
            fixture.write("dst/tree/same.txt", "target\n")
            fixture.write("dst/tree/only-dest.txt", "dest\n")
            _set_mtime(fixture.root / "src/tree/same.txt", 1_700_000_000)
            _set_mtime(fixture.root / "dst/tree/same.txt", 1_700_000_000)
            fixture.dst.mkdir(parents=True, exist_ok=True)
            return [source], fixture.dst
        if case.scenario == "named-dir":
            source = fixture.mkdir("src/tree")
            fixture.write("src/tree/new.txt", "new\n")
            fixture.mkdir("dst")
            return [source], fixture.root / "dst" / "renamed"
        if case.scenario == "file-to-dir":
            source = fixture.write("src/file.txt", "file\n")
            fixture.mkdir("dst")
            return [source], fixture.dst
        if case.scenario == "file-to-file":
            source = fixture.write("src/file.txt", "file\n")
            fixture.mkdir("dst")
            return [source], fixture.root / "dst" / "renamed.txt"
        if case.scenario == "multi-source":
            first = fixture.write("src/one.txt", "one\n")
            second = fixture.write("src/two.txt", "two\n")
            fixture.dst.mkdir(parents=True, exist_ok=True)
            return [first, second], fixture.dst
        raise AssertionError(f"unknown generated case {case.scenario}")

    def test_exhaustive_valid_preview_command_families(self) -> None:
        """Exercise every finite valid Copy planning branch with a stable ID."""

        cases = iter_copy_cases()
        self.assertGreaterEqual(len(cases), 50)
        for case in cases:
            with self.subTest(case=case.case_id):
                fixture = Fixture()
                try:
                    sources, destination = self.setup_generated_case(fixture, case)
                    args = case.args() + (["-L", "64"] if case.verbose else []) + [
                        *(str(source) for source in sources),
                        str(destination),
                    ]
                    result = run_tool("copy", args)
                    message = f"case={case.case_id}\ncommand={' '.join(args)}\n{result.plain}"
                    self.assertEqual(result.returncode, 0, message)
                    self.assertNotIn("Traceback", result.plain, message)
                    expected_rows, expected_bytes = self._expected_generated_rows(sources, destination, case)
                    actual_rows = copy_summary_rows(result.combined)
                    for kind in ("Files", "Dirs"):
                        for header, expected in expected_rows[kind].items():
                            self.assertEqual(actual_rows.get(kind, {}).get(header, 0), expected, message)
                    self.assertEqual(planned_bytes(result.combined), expected_bytes, message)
                    self._assert_generated_tree_contract(result, sources, destination, case)
                    self.assertIn("Type  |", result.plain, message)
                    self.assertNotIn("Proceed with", result.plain, message)
                finally:
                    fixture.close()

    def test_invalid_option_matrix_fails_before_planning(self) -> None:
        invalid_cases = (
            ("sync-and-move", ["--sync", "--move"]),
            ("sync-and-overwrite", ["--sync", "--overwrite"]),
            ("sync-and-collision", ["--sync", "--collision", "source:always"]),
            ("mixed-collision-combinators", ["--collision", "source:newer+larger,size-differs"]),
        )
        for case_id, flags in invalid_cases:
            with self.subTest(case=case_id):
                fixture = self.make_merge_fixture()
                try:
                    result = run_tool(
                        "copy",
                        [*flags, "--preview", fixture.root / "src/tree", fixture.root / "dst"],
                    )
                    self.assertNotEqual(result.returncode, 0, result.plain)
                    self.assertNotIn("Planned transfer bytes:", result.plain)
                    self.assertIn("error", result.plain.lower())
                finally:
                    fixture.close()

    def test_seeded_mixed_fixtures_are_repeatable_and_non_mutating(self) -> None:
        """Exercise deeper mixed states with fixed seeds without flaky randomness."""

        for seed in (0x5F5, 0xA11CE, 0xC0FFEE, 0xD15EA5E):
            with self.subTest(seed=seed):
                fixture = Fixture()
                try:
                    rng = random.Random(seed)
                    fixture.mkdir("src/tree")
                    fixture.mkdir("dst/tree")
                    for index in range(12):
                        name = f"entry-{index:02d}.txt"
                        source = fixture.write(
                            f"src/tree/{name}",
                            "x" * rng.randint(1, 40) + "\n",
                        )
                        if rng.random() < 0.7:
                            destination = fixture.write(
                                f"dst/tree/{name}",
                                "y" * rng.randint(1, 40) + "\n",
                            )
                            timestamp = 1_700_000_000 + rng.randint(0, 4)
                            _set_mtime(source, timestamp)
                            _set_mtime(destination, timestamp + rng.randint(-1, 1))
                    before = snapshot(fixture.root)
                    result = run_tool(
                        "copy",
                        ["--preview", "--showall", "-c", fixture.root / "src/tree", fixture.root / "dst/tree"],
                    )
                    self.assertEqual(result.returncode, 0, result.plain)
                    self.assertIsNotNone(planned_bytes(result.combined), result.plain)
                    self.assertEqual(before, snapshot(fixture.root))
                finally:
                    fixture.close()


if __name__ == "__main__":
    unittest.main()
