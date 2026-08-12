"""Shared deterministic fixtures, process runners, and output tokenizers.

The tests in this package deliberately do not import implementation modules from
the tools.  Their expected values are derived from the fixture itself so that a
traversal or renderer regression cannot make the oracle agree with the bug.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Sequence


REPO_ROOT = Path(__file__).resolve().parents[2]


def _binary(name: str) -> Path:
    override = os.environ.get(f"FSX_{name.upper()}_BIN")
    candidates = [
        Path(override) if override else None,
        REPO_ROOT / "target" / "debug" / name,
        REPO_ROOT / "target" / "release" / name,
        REPO_ROOT / "tools" / name / name,
    ]
    for candidate in candidates:
        if candidate is not None and candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    raise FileNotFoundError(
        f"could not find {name}; build the workspace or set FSX_{name.upper()}_BIN"
    )


BINARIES = {name: (lambda tool_name=name: _binary(tool_name)) for name in ("tree", "twig", "unearth", "copy")}


ANSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
OSC8_RE = re.compile(r"\x1b\]8;;.*?(?:\x1b\\|\x07)")
OSC_RE = re.compile(r"\x1b\].*?(?:\x1b\\|\x07)")


def strip_ansi(text: str) -> str:
    """Remove SGR/control CSI sequences, preserving ordinary text."""

    return ANSI_RE.sub("", text)


def strip_terminal_controls(text: str) -> str:
    """Remove ANSI and OSC sequences for semantic assertions."""

    return OSC_RE.sub("", ANSI_RE.sub("", text))


def osc8_links(text: str) -> list[tuple[str, str]]:
    """Return ``(uri, label)`` pairs from OSC 8 hyperlinks."""

    links: list[tuple[str, str]] = []
    pattern = re.compile(r"\x1b\]8;;([^\x07\x1b]*)(?:\x07|\x1b\\)(.*?)(?:\x1b\]8;;(?:\x07|\x1b\\))")
    for match in pattern.finditer(text):
        links.append((match.group(1), strip_terminal_controls(match.group(2))))
    return links


def normalize_root(text: str, root: Path) -> str:
    """Make temporary fixture paths stable in failure messages."""

    return text.replace(str(root), "<ROOT>")


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: str
    stderr: str

    @property
    def combined(self) -> str:
        return f"{self.stdout}\n{self.stderr}".strip()

    @property
    def plain(self) -> str:
        return strip_terminal_controls(self.combined)


def run_tool(
    name: str,
    args: Sequence[str | bytes | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    input_text: str = "",
    timeout: float = 20,
) -> CommandResult:
    """Run a tool in an isolated, reproducible terminal environment."""

    merged = os.environ.copy()
    merged.update(
        {
            "LC_ALL": "C",
            "LANG": "C",
            "TZ": "UTC",
            "COLUMNS": "120",
            "LINES": "40",
            "TERM": "xterm-256color",
            "COPY_RS_DISABLE_ETA_PRIORS": "1",
            "NO_COLOR": "",
        }
    )
    if env:
        merged.update(env)
    binary = BINARIES[name]()
    cache_dir = tempfile.TemporaryDirectory(prefix="fsx-output-contract-cache-")
    state_dir = tempfile.TemporaryDirectory(prefix="fsx-output-contract-state-")
    merged.setdefault("XDG_CACHE_HOME", cache_dir.name)
    merged.setdefault("XDG_STATE_HOME", state_dir.name)
    try:
        proc = subprocess.run(
            [
                str(binary),
                *(
                    arg if isinstance(arg, (str, bytes)) else os.fspath(arg)
                    for arg in args
                ),
            ],
            cwd=str(cwd) if cwd else None,
            input=input_text,
            text=True,
            capture_output=True,
            env=merged,
            timeout=timeout,
        )
    finally:
        state_dir.cleanup()
        cache_dir.cleanup()
    return CommandResult(proc.returncode, proc.stdout, proc.stderr)


class Fixture:
    """A deterministic filesystem fixture used by all four tool suites."""

    def __init__(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="fsx-output-contract-")
        self.root = Path(self._tmp.name)
        self.src = self.root / "src"
        self.dst = self.root / "dst"

    def close(self) -> None:
        self._tmp.cleanup()

    def __enter__(self) -> "Fixture":
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()

    def write(self, relative: str, content: str = "x\n", *, executable: bool = False) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        if executable:
            path.chmod(0o755)
        return path

    def mkdir(self, relative: str) -> Path:
        path = self.root / relative
        path.mkdir(parents=True, exist_ok=True)
        return path

    def symlink(self, relative: str, target: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.symlink_to(target)
        return path

    def hardlink(self, relative: str, target: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        os.link(self.root / target, path)
        return path

    def mtime(self, relative: str, timestamp: int) -> None:
        path = self.root / relative
        os.utime(path, (timestamp, timestamp), follow_symlinks=False)

    def canonical(self) -> Path:
        return self.root.resolve()

    def populate(self) -> None:
        """Create the common fixture used by semantic output tests."""

        self.mkdir("src/empty")
        self.mkdir("src/sub/deep")
        self.write("src/a.txt", "alpha\n")
        self.write("src/sub/b.txt", "bravo\n")
        self.write("src/run.sh", "#!/bin/sh\n", executable=True)
        self.write("src/.hidden", "hidden\n")
        self.write("src/space name.txt", "space\n")
        self.write("src/leading-dash", "dash\n")
        self.symlink("src/link", "a.txt")
        self.symlink("src/dir-link", "sub")
        self.symlink("src/dangling", "missing-target")
        self.hardlink("src/sub/a-hardlink.txt", "src/a.txt")
        self.mtime("src/a.txt", 1_700_000_000)
        self.mtime("src/sub/b.txt", 1_700_000_100)
        self.mtime("src/run.sh", 1_700_000_200)
        self.mkdir("dst")


def visible_names(path: Path, *, include_hidden: bool = False) -> set[str]:
    """Independent directory listing oracle based on lstat/scandir."""

    names = set()
    for entry in os.scandir(path):
        if not include_hidden and entry.name.startswith("."):
            continue
        names.add(entry.name)
    return names


def output_lines(result: CommandResult) -> list[str]:
    return [line for line in result.plain.splitlines() if line.strip()]


@dataclass(frozen=True)
class RenderedTreeEntry:
    """One parsed tree row, retaining hierarchy and the rendered classifier."""

    depth: int
    name: str
    classifier: str = ""

    @property
    def path_parts(self) -> tuple[str, ...]:
        return tuple(self.name.split("/"))


def parse_tree_entries(result: CommandResult, *, stop_at_tables: bool = True) -> list[RenderedTreeEntry]:
    """Parse branch rows without collapsing them into a name set."""

    entries: list[RenderedTreeEntry] = []
    for raw in strip_terminal_controls(result.combined).splitlines():
        line = raw.rstrip("\n")
        if stop_at_tables and line.startswith("Type  |"):
            break
        match = re.match(r"^(?P<indent>(?:(?:│   |    ))*)(?:├──|└──|┌──)\s?(?P<value>.*)$", line)
        if not match:
            continue
        indent = match.group("indent")
        value = match.group("value").rstrip()
        classifier = value[-1] if value and value[-1] in "/@*=>|" else ""
        if classifier:
            value = value[:-1]
        if value and not value.startswith("..."):
            entries.append(RenderedTreeEntry(len(indent) // 4, value, classifier))
    return entries


def tree_paths(result: CommandResult) -> list[tuple[str, ...]]:
    """Return parsed hierarchical paths in display order."""

    paths: list[tuple[str, ...]] = []
    ancestors: list[str] = []
    for entry in parse_tree_entries(result):
        ancestors = ancestors[: entry.depth]
        ancestors.append(entry.name)
        paths.append(tuple(ancestors))
    return paths


def rendered_tree_paths(entries: Sequence[RenderedTreeEntry]) -> list[tuple[str, ...]]:
    """Build hierarchical paths from an already parsed tree entry stream."""

    paths: list[tuple[str, ...]] = []
    ancestors: list[str] = []
    for entry in entries:
        ancestors = ancestors[: entry.depth]
        ancestors.append(entry.name)
        paths.append(tuple(ancestors))
    return paths


def tree_entry_names(result: CommandResult) -> set[str]:
    """Extract leaf names from Tree-style ``├──``/``└──`` rows."""

    return {entry.name for entry in parse_tree_entries(result)}


def copy_summary_rows(text: str) -> dict[str, dict[str, int]]:
    """Parse Copy's ``Files``/``Dirs`` summary tables into dictionaries."""

    rows: dict[str, dict[str, int]] = {}
    headers: list[str] | None = None
    for raw in strip_terminal_controls(text).splitlines():
        line = raw.strip()
        if "|" not in line:
            continue
        cells = [cell.strip() for cell in line.split("|")]
        if cells and cells[0] == "Type":
            headers = cells[1:]
            continue
        if headers and cells and cells[0] in {"Files", "Dirs"}:
            values: dict[str, int] = {}
            for header, cell in zip(headers, cells[1:]):
                digits = re.sub(r"[^0-9]", "", cell)
                values[header] = int(digits or "0")
            rows.setdefault(cells[0], {}).update(values)
    return rows


def planned_bytes(text: str) -> int | None:
    match = re.search(r"Planned transfer bytes:\s*([0-9,]+)", strip_terminal_controls(text))
    return int(match.group(1).replace(",", "")) if match else None


def copy_preview_names(text: str) -> set[str]:
    """Extract displayed Copy preview names while ignoring table rows."""

    result = CommandResult(0, text, "")
    return {entry.name for entry in parse_tree_entries(result)}


def copy_preview_tree(text: str) -> list[RenderedTreeEntry]:
    """Parse Copy's preview hierarchy before the summary tables."""

    return parse_tree_entries(CommandResult(0, text, ""))


def snapshot(path: Path) -> dict[str, tuple[str, int, int]]:
    """Return a stable lstat snapshot for preview non-mutation assertions."""

    result: dict[str, tuple[str, int, int]] = {}
    for base, dirs, files in os.walk(path, topdown=True, followlinks=False):
        base_path = Path(base)
        for name in [*dirs, *files]:
            item = base_path / name
            stat = item.lstat()
            if item.is_symlink():
                kind = "symlink:" + os.readlink(item)
            elif item.is_dir():
                kind = "dir"
            elif item.is_file():
                kind = "file"
            else:
                kind = "other"
            result[str(item.relative_to(path))] = (kind, stat.st_size, stat.st_mtime_ns)
    return result


def assert_no_mutation(testcase, before: dict, after: dict) -> None:
    testcase.assertEqual(before, after, "preview changed the fixture filesystem")


def remove_tree(path: Path) -> None:
    """Remove a fixture path in tests without following symlinks."""

    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.exists():
        shutil.rmtree(path)
