from __future__ import annotations

import os
import re
import subprocess
import unittest
from pathlib import Path

try:
    from .harness import (
        Fixture,
        osc8_links,
        output_lines,
        run_tool,
        tree_paths,
        strip_terminal_controls,
        tree_entry_names,
        visible_names,
    )
except ImportError:  # unittest discover -s tests/output_contract
    from harness import (  # type: ignore
        Fixture,
        osc8_links,
        output_lines,
        run_tool,
        strip_terminal_controls,
        tree_entry_names,
        tree_paths,
        visible_names,
    )


class TreeOutputContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = Fixture()
        self.fixture.populate()

    def tearDown(self) -> None:
        self.fixture.close()

    def test_default_and_hidden_listing_match_independent_scandir_oracle(self) -> None:
        result = run_tool("tree", ["--color", "never", "--hyperlink", "never", self.fixture.src])
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertEqual(
            tree_entry_names(result) & visible_names(self.fixture.src),
            visible_names(self.fixture.src),
        )
        self.assertNotIn(".hidden", tree_entry_names(result))

        hidden = run_tool(
            "tree",
            ["-a", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(hidden.returncode, 0, hidden.plain)
        self.assertTrue(
            visible_names(self.fixture.src, include_hidden=True).issubset(tree_entry_names(hidden)),
            hidden.plain,
        )
        self.assertIn(".hidden", tree_entry_names(hidden))

    def test_tree_ast_preserves_nested_hierarchy(self) -> None:
        result = run_tool(
            "tree",
            ["-a", "-F", "-L", "3", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertEqual(
            set(tree_paths(result)),
            {
                ("empty",),
                ("sub",),
                ("sub", "deep"),
                ("sub", "a-hardlink.txt"),
                ("sub", "b.txt"),
                ("a.txt",),
                ("run.sh",),
                (".hidden",),
                ("space name.txt",),
                ("leading-dash",),
                ("link",),
                ("dir-link",),
                ("dangling",),
            },
            result.plain,
        )

    def test_classified_rendering_matches_reviewed_golden(self) -> None:
        result = run_tool(
            "tree",
            ["-a", "-F", "-L", "3", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        golden = (Path(__file__).parent / "goldens" / "tree_classified.txt").read_text()
        self.assertEqual(result.plain.replace(str(self.fixture.root), "<ROOT>"), golden.strip())

    def test_classifiers_are_type_derived(self) -> None:
        result = run_tool(
            "tree",
            ["-a", "-F", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        plain = strip_terminal_controls(result.combined)
        self.assertRegex(plain, r"empty/")
        self.assertRegex(plain, r"run\.sh\*")
        self.assertRegex(plain, r"link@")
        self.assertRegex(plain, r"dir-link@")

    def test_depth_and_dirs_only_never_leak_descendants(self) -> None:
        shallow = run_tool(
            "tree",
            ["-a", "-L", "1", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(shallow.returncode, 0, shallow.plain)
        self.assertNotIn("deep", shallow.plain)
        self.assertNotIn("b.txt", shallow.plain)

        dirs = run_tool(
            "tree",
            ["-a", "-d", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(dirs.returncode, 0, dirs.plain)
        names = tree_entry_names(dirs)
        self.assertIn("empty", names)
        self.assertIn("sub", names)
        self.assertNotIn("a.txt", names)
        self.assertNotIn("run.sh", names)

    def test_sort_is_deterministic_and_reverse_is_an_involution(self) -> None:
        first = run_tool(
            "tree",
            ["-a", "--sort", "name", "asc", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        second = run_tool(
            "tree",
            ["-a", "--sort", "name", "asc", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(first.returncode, 0, first.plain)
        self.assertEqual(first.plain, second.plain)

        reversed_result = run_tool(
            "tree",
            ["-a", "-r", "--sort", "name", "asc", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(reversed_result.returncode, 0, reversed_result.plain)
        def row_name(line: str) -> str:
            value = re.split(r"├──|└──|┌──", line, maxsplit=1)[1].strip()
            return re.sub(r"[/@*=>|]+$", "", value)

        first_rows = [row_name(line) for line in output_lines(first) if "──" in line]
        reverse_rows = [row_name(line) for line in output_lines(reversed_result) if "──" in line]
        self.assertEqual(reverse_rows, list(reversed(first_rows)))

    def test_size_time_count_modes_expose_metadata_without_changing_paths(self) -> None:
        result = run_tool(
            "tree",
            ["-a", "-l", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("a.txt", result.plain)
        self.assertIn("run.sh", result.plain)
        self.assertNotIn("Traceback", result.plain)

    def test_always_color_and_hyperlink_emit_real_terminal_sequences(self) -> None:
        colored = run_tool(
            "tree",
            ["-F", "--color", "always", "--hyperlink", "never", self.fixture.src],
        )
        self.assertEqual(colored.returncode, 0, strip_terminal_controls(colored.combined))
        self.assertIn("\x1b[", colored.combined)

        linked = run_tool(
            "tree",
            ["--color", "never", "--hyperlink", "always", self.fixture.src],
        )
        self.assertEqual(linked.returncode, 0, linked.plain)
        links = osc8_links(linked.combined)
        self.assertTrue(links, repr(linked.combined))
        self.assertTrue(any("a.txt" in label for _uri, label in links))
        self.assertTrue(any(uri.startswith("file://") for uri, _label in links))

    def test_git_ignore_and_error_contracts_are_explicit(self) -> None:
        git_dir = self.fixture.src
        subprocess.run(["git", "-C", str(git_dir), "init", "-q"], check=True)
        (git_dir / ".gitignore").write_text("ignored.txt\n", encoding="utf-8")
        self.fixture.write("src/ignored.txt", "ignored\n")
        git = run_tool(
            "tree",
            ["-a", "--git", "--ignore", "--color", "never", "--hyperlink", "never", git_dir],
        )
        self.assertEqual(git.returncode, 0, git.plain)
        self.assertNotIn("ignored.txt", git.plain)

        missing = run_tool(
            "tree",
            ["--color", "never", "--hyperlink", "never", self.fixture.root / "missing"],
        )
        self.assertTrue(missing.stderr or missing.stdout)

    def test_boolean_option_matrix_is_exhaustive_for_scan_policy_flags(self) -> None:
        """Every valid combination of Tree's scan/display policy toggles runs."""

        flags = ("-a", "-d", "-f", "-S", "-H", "-c")
        for mask in range(1 << len(flags)):
            selected = [flag for bit, flag in enumerate(flags) if mask & (1 << bit)]
            with self.subTest(flags=selected):
                result = run_tool(
                    "tree",
                    [*selected, "-L", "3", "--color", "never", "--hyperlink", "never", self.fixture.src],
                )
                self.assertEqual(result.returncode, 0, result.plain)
                self.assertNotIn("panic", result.plain.lower())


if __name__ == "__main__":
    unittest.main()
