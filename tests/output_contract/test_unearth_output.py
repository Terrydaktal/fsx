from __future__ import annotations

import re
import tempfile
import unittest

try:
    from .harness import Fixture, osc8_links, output_lines, run_tool, strip_terminal_controls
except ImportError:  # unittest discover -s tests/output_contract
    from harness import Fixture, osc8_links, output_lines, run_tool, strip_terminal_controls  # type: ignore


class UnearthOutputContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = Fixture()
        self.fixture.populate()

    def tearDown(self) -> None:
        self.fixture.close()

    def result_paths(self, result) -> set[str]:
        return {
            line.strip()
            for line in output_lines(result)
            if line.strip() and not re.match(r"^\s*\d+\s+", line)
        }

    def test_plain_search_is_an_independent_recursive_result_set(self) -> None:
        result = run_tool("unearth", ["--color=never", "a", self.fixture.src])
        self.assertEqual(result.returncode, 0, result.plain)
        paths = self.result_paths(result)
        expected = {
            str(self.fixture.src / "a.txt"),
            str(self.fixture.src / "sub" / "a-hardlink.txt"),
            str(self.fixture.src / "leading-dash"),
            str(self.fixture.src / "dangling"),
            str(self.fixture.src / "space name.txt"),
        }
        self.assertEqual(paths, expected, result.plain)
        self.assertNotIn(str(self.fixture.src / ".hidden"), paths)

    def test_full_regex_and_type_filters_are_composable(self) -> None:
        full = run_tool(
            "unearth",
            ["--full", "--color=never", "txt", self.fixture.src],
        )
        self.assertEqual(full.returncode, 0, full.plain)
        self.assertIn("a.txt", full.plain)

        regex = run_tool(
            "unearth",
            ["--regex", "--color=never", r"^a", self.fixture.src],
        )
        self.assertEqual(regex.returncode, 0, regex.plain)
        self.assertIn("a.txt", regex.plain)
        self.assertNotIn("b.txt", regex.plain)

        files = run_tool(
            "unearth",
            ["--file", "--color=never", "link", self.fixture.src],
        )
        self.assertEqual(files.returncode, 0, files.plain)
        self.assertIn("link", files.plain)
        dirs = run_tool(
            "unearth",
            ["--dir", "--color=never", "sub", self.fixture.src],
        )
        self.assertEqual(dirs.returncode, 0, dirs.plain)
        self.assertIn("sub/", dirs.plain)

    def test_recurse_hidden_and_limit_boundaries(self) -> None:
        no_recurse = run_tool(
            "unearth",
            ["--no-recurse", "--color=never", "b", self.fixture.src],
        )
        self.assertEqual(no_recurse.returncode, 0, no_recurse.plain)
        self.assertNotIn("b.txt", no_recurse.plain)

        hidden = run_tool(
            "unearth",
            ["--hidden", "--color=never", "hidden", self.fixture.src],
        )
        self.assertEqual(hidden.returncode, 0, hidden.plain)
        self.assertIn(".hidden", hidden.plain)

        limited = run_tool(
            "unearth",
            ["--limit", "1", "--color=never", "a", self.fixture.src],
        )
        self.assertEqual(limited.returncode, 0, limited.plain)
        listed = [line for line in output_lines(limited) if str(self.fixture.src) in line]
        self.assertLessEqual(len(listed), 1)

    def test_counts_and_sizes_have_machine_parseable_columns(self) -> None:
        counts = run_tool(
            "unearth",
            ["--counts", "--color=never", "a", self.fixture.src],
        )
        self.assertEqual(counts.returncode, 0, counts.plain)
        self.assertRegex(counts.plain, r"\b\d+\s+.*src")

        sizes = run_tool(
            "unearth",
            ["--sizes", "--color=never", "a", self.fixture.src],
        )
        self.assertEqual(sizes.returncode, 0, sizes.plain)
        for line in output_lines(sizes):
            self.assertRegex(line, r"^[0-9.]+[A-Za-z-]+\t")

    def test_sort_limit_and_reverse_are_deterministic(self) -> None:
        normal = run_tool(
            "unearth",
            ["--sort", "name", "asc", "--color=never", "a", self.fixture.src],
        )
        repeated = run_tool(
            "unearth",
            ["--sort", "name", "asc", "--color=never", "a", self.fixture.src],
        )
        reverse = run_tool(
            "unearth",
            ["--sort", "name", "asc", "--reverse", "--color=never", "a", self.fixture.src],
        )
        self.assertEqual(normal.returncode, 0, normal.plain)
        self.assertEqual(normal.plain, repeated.plain)
        self.assertEqual(reverse.returncode, 0, reverse.plain)
        self.assertNotEqual(normal.plain, reverse.plain)

    def test_hyperlinks_preserve_pcmanfm_parent_selection_target(self) -> None:
        result = run_tool(
            "unearth",
            ["--hyperlink", "--color=never", "a.txt", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, strip_terminal_controls(result.combined))
        links = osc8_links(result.combined)
        self.assertGreaterEqual(len(links), 2, repr(result.combined))
        self.assertTrue(any("?select=" in uri for uri, _label in links))
        self.assertTrue(any(uri.endswith("/a.txt") for uri, _label in links))

    def test_highlighting_is_an_escape_only_presentation_change(self) -> None:
        plain = run_tool("unearth", ["--color=never", "a.txt", self.fixture.src])
        highlighted = run_tool(
            "unearth",
            ["--highlight-match", "--color=always", "a.txt", self.fixture.src],
        )
        self.assertEqual(plain.returncode, 0, plain.plain)
        self.assertEqual(highlighted.returncode, 0, highlighted.plain)
        self.assertEqual(strip_terminal_controls(plain.combined), strip_terminal_controls(highlighted.combined))
        self.assertIn("\x1b[", highlighted.combined)

    def test_invalid_root_has_nonzero_status_and_diagnostic(self) -> None:
        result = run_tool(
            "unearth",
            ["--color=never", "needle", self.fixture.root / "missing"],
        )
        # Explicit unreadable/missing roots fail closed: they must not silently
        # look like a complete empty search or fall back to a global scan.
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("results are incomplete", result.plain)

    def test_contains_all_and_path_scope_do_not_confuse_words_with_roots(self) -> None:
        result = run_tool(
            "unearth",
            ["--contains-all", "a", "hardlink", "--color=never", "--path", str(self.fixture.src)],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("a-hardlink.txt", result.plain)
        self.assertNotIn("b.txt", result.plain)

    def test_index_binary_records_are_length_framed_and_round_trip_paths(self) -> None:
        with tempfile.TemporaryDirectory(prefix="fsx-unearth-index-") as cache:
            env = {"XDG_CACHE_HOME": cache}
            result = run_tool(
                "unearth",
                ["--index-refresh", str(self.fixture.src)],
                env=env,
                timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.plain)
            binary = run_tool(
                "unearth",
                ["--index", "--index-binary", "a.txt", str(self.fixture.src)],
                env=env,
            )
            self.assertEqual(binary.returncode, 0, binary.plain)
            raw = binary.stdout.encode()
        self.assertGreaterEqual(len(raw), 4)
        length = int.from_bytes(raw[:4], "little")
        self.assertGreaterEqual(length, len("a.txt"))
        self.assertGreaterEqual(len(raw), 4 + length)


if __name__ == "__main__":
    unittest.main()
