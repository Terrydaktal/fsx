#!/usr/bin/env python3
import re
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
COPY_BIN = ROOT / "copy"
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def strip_ansi(text):
    return ANSI_RE.sub("", text)


def run_copy(args, cwd=None):
    proc = subprocess.run(
        [str(COPY_BIN), *args],
        cwd=str(cwd) if cwd else None,
        input="n\n",
        text=True,
        capture_output=True,
    )
    combined = f"{proc.stdout}\n{proc.stderr}".strip()
    return proc.returncode, strip_ansi(combined), proc.stdout


def write_file(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def setup_scenario(root, scenario):
    if scenario == "dir_to_existing_dir":
        src = root / "src" / "A"
        dst = root / "dst"
        write_file(src / "f.txt", "x\n")
        write_file(dst / "keep.txt", "k\n")
        return str(src), str(dst), None

    if scenario == "dir_to_existing_named_path":
        src = root / "src" / "A"
        dst = root / "dst" / "A"
        write_file(src / "new.txt", "n\n")
        write_file(dst / "old.txt", "o\n")
        return str(src), str(dst), None

    if scenario == "dir_to_new_named_path":
        src = root / "src" / "A"
        dst = root / "dst" / "newA"
        write_file(src / "f.txt", "x\n")
        (root / "dst").mkdir(parents=True, exist_ok=True)
        return str(src), str(dst), None

    if scenario == "file_to_existing_dir":
        src = root / "src" / "f.txt"
        dst = root / "dst"
        write_file(src, "x\n")
        write_file(dst / "keep.txt", "k\n")
        return str(src), str(dst), None

    if scenario == "file_to_new_file":
        src = root / "src" / "f.txt"
        dst = root / "dst" / "renamed.txt"
        write_file(src, "x\n")
        (root / "dst").mkdir(parents=True, exist_ok=True)
        return str(src), str(dst), None

    if scenario == "parent_edge_poo_to_parent":
        cwd = root / "TelegramBackup" / "poo"
        write_file(cwd / "poo" / "inner.txt", "x\n")
        write_file(cwd / "keep.txt", "k\n")
        return "poo", "..", cwd

    raise ValueError(f"Unknown scenario: {scenario}")


class CopyCliMatrixTests(unittest.TestCase):
    pass


def _make_matrix_test(scenario, move, overwrite, contents, verbose):
    def test(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            src_arg, dst_arg, cwd = setup_scenario(root, scenario)
            args = []
            if move:
                args.append("-m")
            if overwrite:
                args.append("-o")
            if contents:
                args.append("-c")
            if verbose:
                args.append("-v")
            args.extend([src_arg, dst_arg])

            rc, out, raw = run_copy(args, cwd=cwd)
            self.assertEqual(
                rc,
                0,
                msg=f"scenario={scenario} flags={args}\nOUTPUT:\n{out}\nRAW:\n{raw}",
            )
            self.assertNotIn("Traceback", out)
            self.assertIn("Planned transfer bytes:", out)
            no_changes = ("No changes detected; nothing to" in out)
            if not no_changes:
                if move:
                    self.assertIn("Proceed with move? [Y/n]:", out)
                else:
                    self.assertIn("Proceed with copy? [Y/n]:", out)
            if verbose:
                self.assertTrue(
                    ("... and " in out)
                    or ("(no new additions)" in out)
                    or ("No changes detected; nothing to" in out)
                    or ("├──" in out)
                    or ("└──" in out),
                    msg=f"expected verbose tree details or summary, got:\n{out}",
                )

    return test


_SCENARIOS = [
    "dir_to_existing_dir",
    "dir_to_existing_named_path",
    "dir_to_new_named_path",
    "file_to_existing_dir",
    "file_to_new_file",
    "parent_edge_poo_to_parent",
]

for _scenario in _SCENARIOS:
    for _move in (False, True):
        for _overwrite in (False, True):
            for _contents in (False, True):
                for _verbose in (False, True):
                    _name = (
                        f"test_matrix_{_scenario}_"
                        f"{'move' if _move else 'copy'}_"
                        f"{'ovw' if _overwrite else 'noovw'}_"
                        f"{'contents' if _contents else 'ncontents'}_"
                        f"{'verbose' if _verbose else 'nverbose'}"
                    )
                    setattr(
                        CopyCliMatrixTests,
                        _name,
                        _make_matrix_test(
                            _scenario,
                            _move,
                            _overwrite,
                            _contents,
                            _verbose,
                        ),
                    )


if __name__ == "__main__":
    unittest.main(verbosity=2)
