from __future__ import annotations

import unittest

try:
    from .harness import Fixture, osc8_links, run_tool, strip_terminal_controls
except ImportError:  # unittest discover -s tests/output_contract
    from harness import Fixture, osc8_links, run_tool, strip_terminal_controls  # type: ignore


class CrossToolOutputContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = Fixture()
        self.fixture.populate()

    def tearDown(self) -> None:
        self.fixture.close()

    def test_tree_and_twig_agree_on_file_type_classifiers(self) -> None:
        tree = run_tool(
            "tree",
            ["-a", "-F", "--color", "never", "--hyperlink", "never", self.fixture.src],
        )
        twig = run_tool(
            "twig",
            ["-a", "-L", "-F", "--color", "never", "--hyperlink=never", self.fixture.src],
        )
        self.assertEqual(tree.returncode, 0, tree.plain)
        self.assertEqual(twig.returncode, 0, twig.plain)
        tree_plain = strip_terminal_controls(tree.combined)
        twig_plain = strip_terminal_controls(twig.combined)
        for name, suffix in (("empty", "/"), ("run.sh", "*"), ("link", "@"), ("dir-link", "@")):
            self.assertIn(name + suffix, tree_plain)
            self.assertIn(name + suffix, twig_plain)

    def test_tree_twig_and_unearth_use_file_hyperlinks(self) -> None:
        tree = run_tool(
            "tree",
            ["--hyperlink", "always", "--color", "never", self.fixture.src],
        )
        twig = run_tool(
            "twig",
            ["-L", "--hyperlink=always", "--color", "never", self.fixture.src],
        )
        unearth = run_tool(
            "unearth",
            ["--hyperlink", "--color=never", "a.txt", self.fixture.src],
        )
        for result in (tree, twig, unearth):
            self.assertEqual(result.returncode, 0, strip_terminal_controls(result.combined))
            links = osc8_links(result.combined)
            self.assertTrue(links, repr(result.combined))
            self.assertTrue(any(uri.startswith("file://") for uri, _label in links))

    def test_copy_preview_paths_are_a_subset_of_the_same_fixture_namespace(self) -> None:
        result = run_tool(
            "copy",
            ["--preview", "--showall", "-c", self.fixture.src, self.fixture.dst],
        )
        self.assertEqual(result.returncode, 0, result.plain)
        self.assertIn("src", result.plain)
        self.assertIn("Planned transfer bytes:", result.plain)


if __name__ == "__main__":
    unittest.main()
