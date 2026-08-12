"""Finite semantic command families used by the Copy contract matrix.

The matrix is exhaustive over option branches that change planning semantics.
Arbitrary path strings and unbounded numeric values are represented separately
by the boundary tests in ``test_copy_preview``.
"""

from __future__ import annotations

from dataclasses import dataclass
from itertools import product


@dataclass(frozen=True)
class CopyCase:
    scenario: str
    mode: str
    contents: bool
    overwrite: bool
    verbose: bool
    preview_kind: str

    @property
    def case_id(self) -> str:
        return "-".join(
            (
                self.scenario,
                self.mode,
                "contents" if self.contents else "merge",
                "overwrite" if self.overwrite else "merge-target",
                "verbose" if self.verbose else "compact",
                self.preview_kind,
            )
        )

    def args(self) -> list[str]:
        flags: list[str] = []
        if self.mode == "move":
            flags.append("--move")
        elif self.mode == "sync":
            flags.append("--sync")
        if self.contents:
            flags.append("--contents-only")
        if self.overwrite:
            flags.append("--overwrite")
        if self.verbose:
            flags.append("--showall")
        flags.append("--preview" if self.preview_kind == "full" else "--preview-lite")
        return flags


def iter_copy_cases() -> list[CopyCase]:
    cases: list[CopyCase] = []
    scenarios = ("new-dir", "merge-dir", "named-dir", "file-to-dir", "file-to-file", "multi-source")
    modes = ("copy", "move", "sync")
    previews = ("full", "lite")
    for scenario, mode, contents, overwrite, verbose, preview_kind in product(
        scenarios,
        modes,
        (False, True),
        (False, True),
        (False, True),
        previews,
    ):
        # A sync is an exact directory operation; the multi-source form is a
        # valid merge/copy family but not a sync source list.
        if scenario == "multi-source" and mode == "sync":
            continue
        if scenario == "multi-source" and contents:
            continue
        if scenario == "multi-source" and overwrite:
            continue
        if mode == "sync" and overwrite:
            continue
        if mode == "sync" and scenario not in {"new-dir", "merge-dir", "named-dir"}:
            continue
        if contents and scenario in {"file-to-dir", "file-to-file"}:
            continue
        if overwrite and scenario in {"file-to-dir", "file-to-file"}:
            continue
        # Overwrite replaces a target and contents-only is a merge operation;
        # the combination is rejected by the CLI and belongs to the invalid
        # option matrix, not this valid-command matrix.
        if contents and overwrite:
            continue
        cases.append(
            CopyCase(scenario, mode, contents, overwrite, verbose, preview_kind)
        )
    return cases
