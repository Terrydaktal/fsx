#!/usr/bin/env python3
"""Process and fault-injection coverage for copy's durability boundaries."""

import os
import shutil
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DIAGNOSTIC_BIN = ROOT.parents[1] / "target" / "diagnostic" / "copy-rs"
COPY_BIN = Path(
    os.environ.get(
        "COPY_RS_COPY_BIN",
        DEFAULT_DIAGNOSTIC_BIN if DEFAULT_DIAGNOSTIC_BIN.exists() else ROOT / "copy",
    )
)


def test_env(state):
    env = os.environ.copy()
    env["XDG_STATE_HOME"] = str(state)
    env["COPY_RS_DISABLE_ETA_PRIORS"] = "1"
    env["COPY_RS_CHUNK_KIB"] = "64"
    return env


class CopyFaultIntegrationTests(unittest.TestCase):
    def test_hard_crash_retains_each_journal_boundary(self):
        for boundary in ("planned", "transferring", "published", "complete"):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as td:
                root = Path(td)
                source = root / "source.txt"
                destination = root / "destination.txt"
                state = root / "state"
                source.write_text("payload", encoding="utf-8")
                env = test_env(state)
                env["COPY_RS_TEST_CRASH_AT"] = boundary
                proc = subprocess.run(
                    [str(COPY_BIN), str(source), str(destination)],
                    input="y\n",
                    text=True,
                    capture_output=True,
                    env=env,
                )
                self.assertEqual(proc.returncode, 86, proc.stdout + proc.stderr)
                journals = list((state / "copy-rs").glob("*.journal"))
                self.assertEqual(len(journals), 1)
                contents = journals[0].read_text(encoding="utf-8")
                self.assertIn(f"state={boundary}", contents)
                self.assertEqual(destination.exists(), boundary in ("published", "complete"))

    def test_hard_crash_retains_failed_journal_boundary(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            source = root / "missing.txt"
            destination = root / "destination.txt"
            state = root / "state"
            source.write_text("payload", encoding="utf-8")
            env = test_env(state)
            env["COPY_RS_TEST_CRASH_AT"] = "failed"
            env["COPY_RS_TEST_HOOK"] = str(root / "remove-source.sh")
            Path(env["COPY_RS_TEST_HOOK"]).write_text(
                "#!/bin/sh\n"
                "if [ \"$COPY_RS_TEST_HOOK_POINT\" = after-preflight-before-execution ]; then\n"
                f"  rm -- '{source}'\n"
                "fi\n",
                encoding="utf-8",
            )
            Path(env["COPY_RS_TEST_HOOK"]).chmod(0o755)
            proc = subprocess.run(
                [str(COPY_BIN), str(source), str(destination)],
                input="y\n",
                text=True,
                capture_output=True,
                env=env,
            )
            self.assertEqual(proc.returncode, 86, proc.stdout + proc.stderr)
            journal = next((state / "copy-rs").glob("*.journal"))
            self.assertIn("state=failed", journal.read_text(encoding="utf-8"))

    def test_hard_crash_covers_atomic_publication_boundaries(self):
        cases = (("before-publication", False), ("after-publication", True))
        for boundary, destination_exists in cases:
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as td:
                root = Path(td)
                source = root / "source.txt"
                destination = root / "destination.txt"
                state = root / "state"
                source.write_text("payload", encoding="utf-8")
                env = test_env(state)
                env["COPY_RS_TEST_CRASH_AT"] = boundary
                proc = subprocess.run(
                    [str(COPY_BIN), str(source), str(destination)],
                    input="y\n",
                    text=True,
                    capture_output=True,
                    env=env,
                )
                self.assertEqual(proc.returncode, 86, proc.stdout + proc.stderr)
                journal = next((state / "copy-rs").glob("*.journal"))
                self.assertIn("state=transferring", journal.read_text(encoding="utf-8"))
                self.assertEqual(destination.exists(), destination_exists)

    def test_sigint_and_sigterm_cancel_a_live_transfer(self):
        for sig in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=sig), tempfile.TemporaryDirectory() as td:
                root = Path(td)
                source = root / "source.bin"
                destination = root / "destination.bin"
                state = root / "state"
                marker = root / "transfer-started"
                with source.open("wb") as stream:
                    stream.truncate(64 * 1024 * 1024)
                env = test_env(state)
                env["COPY_RS_TEST_TRANSFER_MARKER"] = str(marker)
                env["COPY_RS_TEST_TRANSFER_PAUSE_MS"] = "5000"
                proc = subprocess.Popen(
                    [str(COPY_BIN), str(source), str(destination)],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    env=env,
                )
                proc.stdin.write("y\n")
                proc.stdin.flush()
                deadline = time.monotonic() + 10
                while not marker.exists():
                    if proc.poll() is not None or time.monotonic() >= deadline:
                        break
                    time.sleep(0.01)
                proc.send_signal(sig)
                stdout, stderr = proc.communicate(timeout=20)
                self.assertNotEqual(proc.returncode, 0, stdout + stderr)
                self.assertTrue(list((state / "copy-rs").glob("*.journal")))

    def test_mutation_before_publication_copies_opened_snapshot_or_fails(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            source = root / "source.txt"
            destination = root / "destination.txt"
            hook = root / "hook.sh"
            source.write_text("before", encoding="utf-8")
            hook.write_text(
                "#!/bin/sh\n"
                "if [ \"$COPY_RS_TEST_HOOK_POINT\" = before-publication ]; then\n"
                f"  printf '%s' after > '{source}'\n"
                "fi\n",
                encoding="utf-8",
            )
            hook.chmod(0o755)
            env = test_env(root / "state")
            env["COPY_RS_TEST_HOOK"] = str(hook)
            proc = subprocess.run(
                [str(COPY_BIN), "--verify", str(source), str(destination)],
                input="y\n",
                text=True,
                capture_output=True,
                env=env,
            )
            self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertIn("verification failed", proc.stdout + proc.stderr)

    def test_mutation_between_preview_and_preflight_is_rejected_without_writes(self):
        self._assert_source_removal_at_hook("after-preview-before-preflight", 1)

    def test_mutation_between_preflight_and_execution_fails_without_publication(self):
        self._assert_source_removal_at_hook("after-preflight-before-execution", 1)

    def _assert_source_removal_at_hook(self, point, expected_returncode):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            source = root / "source.txt"
            destination = root / "destination.txt"
            hook = root / "hook.sh"
            source.write_text("payload", encoding="utf-8")
            hook.write_text(
                "#!/bin/sh\n"
                f"if [ \"$COPY_RS_TEST_HOOK_POINT\" = '{point}' ]; then\n"
                f"  rm -- '{source}'\n"
                "fi\n",
                encoding="utf-8",
            )
            hook.chmod(0o755)
            env = test_env(root / "state")
            env["COPY_RS_TEST_HOOK"] = str(hook)
            proc = subprocess.run(
                [str(COPY_BIN), str(source), str(destination)],
                input="y\n",
                text=True,
                capture_output=True,
                env=env,
            )
            self.assertEqual(proc.returncode, expected_returncode, proc.stdout + proc.stderr)
            self.assertFalse(destination.exists(), proc.stdout + proc.stderr)

    @unittest.skipUnless(shutil.which("sudo"), "sudo unavailable")
    def test_actual_sudo_invocation(self):
        if not os.environ.get("COPY_RS_RUN_SUDO_TEST"):
            self.skipTest("set COPY_RS_RUN_SUDO_TEST=1 in an interactive terminal")
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            source = root / "source.txt"
            destination = root / "destination.txt"
            source.write_text("privileged", encoding="utf-8")
            proc = subprocess.run(
                [str(COPY_BIN), "--sudo", str(source), str(destination)],
                input="y\n",
                text=True,
                capture_output=True,
                env=test_env(root / "state"),
                timeout=120,
            )
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(destination.read_text(encoding="utf-8"), "privileged")


if __name__ == "__main__":
    unittest.main(verbosity=2)
