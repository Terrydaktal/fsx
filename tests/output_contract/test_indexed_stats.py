"""Daemon/live parity after structural and allocation changes in a private tree."""
from __future__ import annotations

import os
import sqlite3
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

try:
    from .harness import Fixture, REPO_ROOT, run_tool
except ImportError:
    from harness import Fixture, REPO_ROOT, run_tool


class IndexedStatsContractTests(unittest.TestCase):
    def test_indexed_and_live_listing_and_styles_agree_after_updates(self):
        binary = Path(os.environ.get("FSX_FSXD_BIN", REPO_ROOT / "target/debug/fsxd"))
        if not binary.is_file():
            self.skipTest("build fsxd to exercise indexed listing parity")
        fixture = Fixture()
        try:
            fixture.populate()
            with tempfile.TemporaryDirectory(prefix="fsx-index-parity-") as directory:
                cache = Path(directory)
                database = cache / "index.sqlite"
                env = {**os.environ, "XDG_CACHE_HOME": str(cache), "FSX_INDEX_DB": str(database)}
                with tempfile.TemporaryFile(mode="w+") as log:
                    daemon = subprocess.Popen([str(binary), "--threads", "2", str(fixture.src)],
                                              env=env, stdout=log, stderr=log)
                    try:
                        def wait_clean(expected_size=None):
                            deadline = time.monotonic() + 20
                            while time.monotonic() < deadline:
                                if daemon.poll() is not None:
                                    log.seek(0)
                                    self.fail(log.read())
                                if database.exists():
                                    try:
                                        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
                                            ready = db.execute("SELECT COUNT(*) FROM watch_state WHERE status='running' AND dirty=0 AND online=1").fetchone()[0]
                                            updated = expected_size is None or db.execute(
                                                "SELECT COUNT(*) FROM entries e JOIN strings s ON s.id=e.name_id WHERE s.value='perf-fresh' AND e.size=?",
                                                (expected_size,)).fetchone()[0] == 1
                                            if ready and updated:
                                                return
                                    except sqlite3.OperationalError:
                                        pass
                                time.sleep(0.02)
                            log.seek(0)
                            self.fail("index did not converge: " + log.read())

                        for update in (False, True):
                            if update:
                                nested = fixture.src / "target" / "nested"
                                nested.mkdir(parents=True)
                                (nested / "perf-fresh").write_bytes(b"fresh" * 100_000)
                            wait_clean(500_000 if update else None)
                            args = ["-aS", "-c", "-L", "--sort", "name", "--color=never", "--hyperlink=never", fixture.src]
                            indexed = run_tool("twig", args, env=env)
                            live = run_tool("twig", args, env={**env, "FSX_INDEX_DB": str(cache / "absent.sqlite")})
                            self.assertEqual(indexed.returncode, 0, indexed.plain)
                            self.assertEqual(live.returncode, 0, live.plain)
                            self.assertEqual(indexed.stdout, live.stdout)
                            for styles in (["--color=always"], ["--color=always", "--classify", "--hyperlink"]):
                                streamed = run_tool("unearth", ["--index", *styles, "a", fixture.src], env=env)
                                buffered = run_tool("unearth", ["--index", "--sort", "name", "asc", *styles, "a", fixture.src], env=env)
                                self.assertEqual(streamed.returncode, 0, streamed.plain)
                                self.assertEqual(buffered.returncode, 0, buffered.plain)
                                self.assertEqual(sorted(streamed.stdout.splitlines()), sorted(buffered.stdout.splitlines()))
                    finally:
                        if daemon.poll() is None:
                            daemon.terminate()
                            try:
                                daemon.wait(timeout=15)
                            except subprocess.TimeoutExpired:
                                daemon.kill()
                                daemon.wait()
        finally:
            fixture.close()
