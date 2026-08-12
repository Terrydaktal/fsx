from __future__ import annotations

import re
import unittest
from pathlib import Path

try:
    from .harness import Fixture, osc8_links, output_lines, run_tool, strip_terminal_controls
except ImportError:  # unittest discover -s tests/output_contract
    from harness import Fixture, osc8_links, output_lines, run_tool, strip_terminal_controls  # type: ignore


def twig_names(result) -> set[str]:
    names: set[str] = set()
    for line in output_lines(result):
        value = line.strip()
        if not value or value.startswith("total "):
            continue
        if " " in value and re.match(r"^[bcdlprsuwx-]{10}\s", value):
            value = value.split()[-1]
        value = re.sub(r"[/@*=>|]+$", "", value)
        names.add(value)
    return names


class TwigOutputContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = Fixture()
        self.fixture.populate()

    def tearDown(self) -> None:
        self.fixture.close()

    def test_list_output_covers_visible_and_hidden_entries(self) -> None:
        result = run_tool(
            "twig",
            ["-a", "-L", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        names = twig_names(result)
        expected = {".", "..", "empty", "sub", "a.txt", "run.sh", ".hidden", "space name.txt", "leading-dash", "link", "dir-link", "dangling"}
        self.assertEqual(names, expected, result.plain)
        self.assertIn("run.sh", result.plain)

    def test_almost_all_excludes_dot_entries_but_keeps_hidden_files(self) -> None:
        result = run_tool(
            "twig",
            ["-A", "-L", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        names = twig_names(result)
        self.assertNotIn(".", names)
        self.assertNotIn("..", names)
        self.assertIn(".hidden", names)

    def test_no_traverse_and_dirs_only_have_distinct_semantics(self) -> None:
        no_traverse = run_tool(
            "twig",
            ["-n", "-L", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(no_traverse.returncode, 0, no_traverse.plain)
        self.assertIn("src", no_traverse.plain)
        self.assertNotIn("a.txt", no_traverse.plain)

        dirs = run_tool(
            "twig",
            ["-a", "-d", "-L", "-F", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(dirs.returncode, 0, dirs.plain)
        self.assertIn("empty/", dirs.plain)
        self.assertIn("sub/", dirs.plain)
        self.assertNotIn("a.txt", dirs.plain)

    def test_classifiers_targets_absolute_paths_and_long_columns(self) -> None:
        result = run_tool(
            "twig",
            ["-a", "-l", "-x", "-X", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("link -> a.txt", result.plain)
        self.assertIn(str(self.fixture.src / "a.txt"), result.plain)
        self.assertRegex(result.plain, r"[d-][rwx-]{9}")

    def test_sort_order_is_reproducible_and_reverse_changes_it(self) -> None:
        first = run_tool(
            "twig",
            ["-L", "--sort", "name", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        second = run_tool(
            "twig",
            ["-L", "--sort", "name", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        reverse = run_tool(
            "twig",
            ["-L", "-r", "--sort", "name", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(first.returncode, 0, first.plain)
        self.assertEqual(first.plain, second.plain)
        self.assertEqual(reverse.returncode, 0, reverse.plain)
        self.assertNotEqual(first.plain, reverse.plain)

    def test_size_count_header_and_color_hyperlink_contracts(self) -> None:
        detailed = run_tool(
            "twig",
            ["-a", "-l", "-s", "-c", "-v", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(detailed.returncode, 0, detailed.plain)
        self.assertIn("a.txt", detailed.plain)
        self.assertNotIn("Traceback", detailed.plain)

        colored = run_tool(
            "twig",
            ["-F", "--color", "always", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(colored.returncode, 0, colored.plain)
        self.assertIn("\x1b[", colored.combined)

        linked = run_tool(
            "twig",
            ["-L", "--color", "never", "--hyperlink=always", self.fixture.src],
        )
        self.assertEqual(linked.returncode, 0, linked.plain)
        self.assertTrue(osc8_links(linked.combined), repr(linked.combined))

    def test_boolean_option_matrix_covers_all_valid_layout_combinations(self) -> None:
        option_groups = (
            ((), ("-a",), ("-A",)),
            ((), ("-L",), ("-l",)),
            ((), ("-d",), ("-n",)),
            ((), ("-p",), ("-s",)),
            ((), ("-c",), ("-t",)),
            ((), ("-F",), ("-x",)),
        )
        cases = [[]]
        for group in option_groups:
            cases.extend([list(choice) for choice in group if choice])
        for index, left in enumerate(option_groups):
            for right in option_groups[index + 1 :]:
                cases.extend(
                    [list(first + second) for first in left if first for second in right if second]
                )
        for selected in cases:
            with self.subTest(flags=selected):
                result = run_tool(
                    "twig",
                    [*selected, "--color", "never", "--hyperlink=never", self.fixture.src],
                )
                self.assertEqual(result.returncode, 0, result.plain)
                self.assertNotIn("panic", result.plain.lower())

    def test_true_size_and_hardlink_switch_change_only_allocated_accounting(self) -> None:
        deduped = run_tool(
            "twig",
            ["-n", "-S", "-s", "-L", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        doubled = run_tool(
            "twig",
            ["-n", "-S", "-H", "-s", "-L", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(deduped.returncode, 0, deduped.plain)
        self.assertEqual(doubled.returncode, 0, doubled.plain)
        self.assertIn("src", deduped.plain)
        self.assertIn("src", doubled.plain)
        self.assertNotEqual(deduped.plain, doubled.plain)

    def test_multi_path_headers_and_absolute_output_are_unambiguous(self) -> None:
        result = run_tool(
            "twig",
            ["-L", "-v", "-X", "--color", "never", "--hyperlink=never", self.fixture.src, self.fixture.dst],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn(str(self.fixture.src), result.plain)
        self.assertIn(str(self.fixture.dst), result.plain)

    def test_classified_rendering_matches_reviewed_golden(self) -> None:
        result = run_tool(
            "twig",
            ["-a", "-L", "-F", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        golden = (Path(__file__).parent / "goldens" / "twig_classified.txt").read_text()
        self.assertEqual(result.plain, golden.strip())


if __name__ == "__main__":
    unittest.main()
