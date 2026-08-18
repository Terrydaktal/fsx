#!/usr/bin/env python3
"""Small release-vs-diagnostic smoke benchmark for instrumentation isolation."""

from __future__ import annotations

import json
import os
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RELEASE = ROOT / "target" / "release" / "copy-rs"
DIAGNOSTIC = ROOT / "target" / "diagnostic" / "copy-rs"
BASELINE = ROOT / "tools" / "build" / "baselines" / "diagnostic-overhead.json"


def median_preview(binary: Path, source: Path, destination: Path, state: Path) -> float:
    samples = []
    for index in range(5):
        run_state = state / str(index)
        run_state.mkdir(parents=True)
        started = time.perf_counter_ns()
        subprocess.run(
            [str(binary), "--preview", str(source), str(destination)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={
                **os.environ,
                "XDG_STATE_HOME": str(run_state),
                "COPY_RS_DISABLE_ETA_PRIORS": "1",
            },
        )
        samples.append((time.perf_counter_ns() - started) / 1_000_000)
    return statistics.median(samples)


def main() -> int:
    if not RELEASE.is_file() or not DIAGNOSTIC.is_file():
        raise SystemExit("build release and diagnostic Copy binaries first")
    if b"COPY_RS_TEST_CRASH_AT" in RELEASE.read_bytes():
        raise SystemExit("release binary contains diagnostic crash hooks")
    if b"COPY_RS_TEST_CRASH_AT" not in DIAGNOSTIC.read_bytes():
        raise SystemExit("diagnostic binary is missing its fault hooks")

    with tempfile.TemporaryDirectory(prefix="fsx-overhead-") as directory:
        root = Path(directory)
        source = root / "source"
        source.mkdir()
        for index in range(32):
            (source / f"file-{index}").write_bytes(b"payload" * 128)
        release_ms = median_preview(RELEASE, source, root / "release-dest", root / "release-state")
        diagnostic_ms = median_preview(
            DIAGNOSTIC, source, root / "diagnostic-dest", root / "diagnostic-state"
        )

    ratio = ((release_ms / diagnostic_ms) - 1.0) * 100.0 if diagnostic_ms else 0.0
    baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
    maximum = float(baseline["max_release_vs_diagnostic_percent"])
    if ratio > maximum:
        raise SystemExit(
            f"release diagnostic overhead {ratio:.2f}% exceeds baseline allowance {maximum:.2f}%"
        )
    result = {
        "release_median_ms": round(release_ms, 3),
        "diagnostic_median_ms": round(diagnostic_ms, 3),
        "release_vs_diagnostic_percent": round(ratio, 2),
        "baseline_max_release_vs_diagnostic_percent": maximum,
    }
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
