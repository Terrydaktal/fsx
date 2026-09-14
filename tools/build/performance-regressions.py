#!/usr/bin/env python3
"""Repeatable performance fixtures with correctness checks, not timing gates.

Runs only on freshly created temporary data. No daemon, privilege escalation,
cache dropping or user-tree traversal is needed. wait4 reports child CPU and
peak RSS separately from fixture construction and output verification.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import signal
import statistics
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CONTROLS = re.compile(r"\x1b\].*?(?:\x1b\\|\x07)|\x1b\[[0-?]*[ -/]*[@-~]")


def measure(runner: Path, command: list[str], output_path: Path, error_path: Path, input_path: Path,
            env: dict[str, str]) -> dict[str, float]:
    metrics_path = output_path.with_name("metrics")
    with output_path.open("wb") as output, error_path.open("wb") as error, input_path.open("rb") as stdin:
        with subprocess.Popen([str(runner), str(metrics_path), *command], stdin=stdin, stdout=output,
                              stderr=error, env=env, start_new_session=True) as process:
            try:
                process.wait(timeout=120)
            except subprocess.TimeoutExpired:
                # The unreaped runner owns this newly created process group.
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise
            if process.returncode:
                raise RuntimeError(f"{command}: {error_path.read_text(errors='replace')}")
    user, system, rss, elapsed = metrics_path.read_text().split()
    return {"elapsed_ms": float(elapsed),
            "user_ms": float(user) * 1000, "system_ms": float(system) * 1000,
            "peak_rss_kib": int(rss)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/release")
    parser.add_argument("--files", type=int, default=4096)
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args()
    if args.files < 32 or args.repeat < 1:
        parser.error("--files must be >= 32 and --repeat must be positive")
    binaries = {name: (args.bin_dir / name).resolve(strict=True)
                for name in ("tree", "twig", "unearth", "copy-rs")}
    report = {"files": args.files, "repeat": args.repeat, "binaries": {
        name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        for name, path in binaries.items()}, "workloads": {}}
    with tempfile.TemporaryDirectory(prefix="fsx-perf-") as directory:
        root = Path(directory)
        runner = root / "measure"
        subprocess.run(["cc", "-O2", "-Wall", "-Wextra", "-Werror", str(ROOT / "tools/build/measure.c"),
                        "-o", str(runner)], check=True)
        source = root / "source"
        source.mkdir()
        input_path = root / "stdin"
        input_path.write_text("y\n")
        expected_files = set()
        # Overlapping matching directories, target/ contents, tiny files and
        # sibling hardlinks exercise the optimizations without a content cache.
        for i in range(args.files):
            parent = source / f"bucket-{i // 64:04d}" / "target" / "nested"
            parent.mkdir(parents=True, exist_ok=True)
            path = parent / f"item-{i:06d}"
            path.write_bytes(bytes([i % 251]) * (64 + i % 1024))
            expected_files.add(str(path))
        for i in range(min(64, args.files)):
            path = source / f"hardlink-{i:04d}"
            path.hardlink_to(Path(sorted(expected_files)[i]))
            expected_files.add(str(path))

        state = root / "state"
        state.mkdir()
        env = {**os.environ, "XDG_STATE_HOME": str(state), "XDG_CACHE_HOME": str(root / "cache"),
               "FSX_INDEX_DB": str(root / "missing-index.sqlite3"),
               "COPY_RS_DISABLE_ETA_PRIORS": "1", "LC_ALL": "C", "TERM": "xterm-256color"}
        workloads = {
            "twig-live-sizes": ("twig", ["-aS", "--color=never", str(source)]),
            "tree-truncated-size-counts": ("tree", ["-Sc", "-L", "3", "-T", "4", str(source)]),
            "unearth-styled-files": ("unearth", ["--live", "--file", "--color=always", "*", str(source)]),
            "unearth-overlapping-sizes": ("unearth", ["--live", "--dir", "--sizes", "*", str(source)]),
            "unearth-name-top-k-sizes": ("unearth", ["--live", "--dir", "--sizes", "--sort", "name", "asc", "--limit", "5", "*", str(source)]),
            "copy-preview": ("copy-rs", ["--preview", str(source), str(root / "preview-dest")]),
            "copy-tiny-atomic": ("copy-rs", []),
        }
        for name, (tool, arguments) in workloads.items():
            samples = []
            for iteration in range(args.repeat):
                destination = root / f"copy-dest-{iteration}"
                command = arguments if arguments else [str(source), str(destination)]
                output_path = root / "stdout"
                samples.append(measure(runner, [str(binaries[tool]), *command], output_path,
                                       root / "stderr", input_path, env))
                output_text = CONTROLS.sub("", output_path.read_text())
                if name == "unearth-styled-files":
                    assert set(output_text.splitlines()) == expected_files, name
                elif name == "unearth-overlapping-sizes":
                    listed = [line.split("\t", 1)[1].rstrip("/") for line in output_text.splitlines()]
                    expected = {str(p) for p in source.rglob("*") if p.is_dir()}
                    assert set(listed) == expected, name
                elif name == "unearth-name-top-k-sizes":
                    assert len(output_text.splitlines()) == 5, name
                elif name == "copy-tiny-atomic":
                    copied = {str(p.relative_to(destination)) for p in destination.rglob("*") if p.is_file()}
                    expected = {str(Path(p).relative_to(source)) for p in expected_files}
                    assert copied == expected, name
                    for relative in expected:
                        assert (source / relative).read_bytes() == (destination / relative).read_bytes(), relative
                else:
                    assert output_text.strip(), name
            report["workloads"][name] = {key: round(statistics.median(s[key] for s in samples), 3)
                                         for key in samples[0]}
        assert not (root / "missing-index.sqlite3").exists(), "live workload created an index"
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
