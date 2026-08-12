"""Implementation-independent filesystem models for output-contract tests."""

from __future__ import annotations

import os
import stat
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class ObjectInfo:
    rel: str
    kind: str
    size: int
    mtime_ns: int
    link_target: str | None = None


def _kind(info: os.stat_result) -> str:
    mode = info.st_mode
    if stat.S_ISREG(mode):
        return "file"
    if stat.S_ISDIR(mode):
        return "dir"
    if stat.S_ISLNK(mode):
        return "symlink"
    return "other"


def manifest(root: Path) -> dict[str, ObjectInfo]:
    """Recursively collect lstat metadata without following symlinks."""

    result: dict[str, ObjectInfo] = {}
    if not root.exists() and not root.is_symlink():
        return result
    # Files and symlinks are valid Copy sources/targets too.  Represent the
    # object itself as ``.`` so callers can use the same model for file-to-file
    # and recursive directory cases without accidentally calling scandir on a
    # regular file.
    if root.is_symlink() or not root.is_dir():
        info = root.lstat()
        kind = _kind(info)
        result["."] = ObjectInfo(
            rel=".",
            kind=kind,
            size=info.st_size,
            mtime_ns=info.st_mtime_ns,
            link_target=os.readlink(root) if kind == "symlink" else None,
        )
        return result
    pending = [root]
    while pending:
        current = pending.pop()
        with os.scandir(current) as entries:
            for entry in entries:
                path = Path(entry.path)
                info = path.lstat()
                rel = path.relative_to(root).as_posix()
                kind = _kind(info)
                result[rel] = ObjectInfo(
                    rel=rel,
                    kind=kind,
                    size=info.st_size,
                    mtime_ns=info.st_mtime_ns,
                    link_target=os.readlink(path) if kind == "symlink" else None,
                )
                if kind == "dir":
                    pending.append(path)
    return result


def _relation(source: ObjectInfo, destination: ObjectInfo) -> tuple[str, int]:
    """Return relation label and source bytes to transfer under metadata policy."""

    if source.kind != destination.kind:
        return "type-differs", source.size if source.kind == "file" else 0
    if source.kind != "file":
        return ("same", 0) if source.kind == "dir" else (
            "same" if source.link_target == destination.link_target else "type-differs",
            0,
        )
    if source.mtime_ns == destination.mtime_ns and source.size == destination.size:
        return "same", 0
    return "metadata-differs", source.size


@dataclass(frozen=True)
class TreeExpectation:
    """Expected relation counts for a source/destination tree pair."""

    new_files: int
    new_dirs: int
    modified_files: int
    modified_dirs: int
    identical_files: int
    identical_dirs: int
    uncollided_files: int
    uncollided_dirs: int
    deleted_files: int
    deleted_dirs: int
    planned_bytes: int

    @classmethod
    def compare(cls, source: Path, destination: Path, *, sync: bool = False) -> "TreeExpectation":
        src = manifest(source)
        dst = manifest(destination)
        counts = {
            "new_files": 0,
            "new_dirs": 0,
            "modified_files": 0,
            "modified_dirs": 0,
            "identical_files": 0,
            "identical_dirs": 0,
            "uncollided_files": 0,
            "uncollided_dirs": 0,
            "deleted_files": 0,
            "deleted_dirs": 0,
        }
        planned = 0
        type_conflicts = {
            rel
            for rel in src.keys() & dst.keys()
            if src[rel].kind != dst[rel].kind
        }
        for rel in sorted(src.keys() | dst.keys()):
            if any(rel.startswith(conflict + "/") for conflict in type_conflicts):
                # A file/dir/symlink replacement owns the whole destination
                # subtree; descendants are not independent uncollided rows.
                continue
            source_info = src.get(rel)
            destination_info = dst.get(rel)
            if source_info is None:
                key = "deleted_" if sync else "uncollided_"
                suffix = "files" if destination_info.kind == "file" else "dirs" if destination_info.kind == "dir" else "files"
                counts[key + suffix] += 1
                continue
            if destination_info is None:
                suffix = "files" if source_info.kind == "file" else "dirs" if source_info.kind == "dir" else "files"
                counts["new_" + suffix] += 1
                planned += source_info.size if source_info.kind == "file" else 0
                continue
            # Copy's relation summary intentionally reports regular files and
            # directories; an unchanged symlink is rendered in the tree but is
            # not included in either summary row.
            if source_info.kind == "symlink" and destination_info.kind == "symlink":
                if source_info.link_target == destination_info.link_target:
                    continue
            if source_info.kind != destination_info.kind and destination_info.kind in {"file", "dir"}:
                # The preview exposes the source replacement as modified and
                # the displaced destination object as uncollided.
                displaced = "files" if destination_info.kind == "file" else "dirs"
                counts["uncollided_" + displaced] += 1
            relation, bytes_to_copy = _relation(source_info, destination_info)
            if relation == "same":
                suffix = "files" if source_info.kind == "file" else "dirs" if source_info.kind == "dir" else "files"
                counts["identical_" + suffix] += 1
            else:
                suffix = "files" if source_info.kind == "file" else "dirs" if source_info.kind == "dir" else "files"
                counts["modified_" + suffix] += 1
                planned += bytes_to_copy
        return cls(planned_bytes=planned, **counts)
