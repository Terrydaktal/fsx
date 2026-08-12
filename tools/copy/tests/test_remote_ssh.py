#!/usr/bin/env python3
"""Real localhost sshd + remote-rsync integration coverage."""

import os
import pwd
import shutil
import socket
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
COPY_BIN = Path(os.environ.get("COPY_RS_COPY_BIN", ROOT / "copy"))


@unittest.skipUnless(shutil.which("sshd") and shutil.which("rsync"), "sshd/rsync unavailable")
class RemoteSshIntegrationTests(unittest.TestCase):
    def test_local_to_remote_and_remote_to_local_rsync(self):
        with tempfile.TemporaryDirectory(prefix="copy-rs-ssh-") as td:
            root = Path(td)
            host_key = root / "host_key"
            client_key = root / "client_key"
            authorized_keys = root / "authorized_keys"
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(host_key)], check=True)
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(client_key)], check=True)
            authorized_keys.write_bytes((root / "client_key.pub").read_bytes())
            authorized_keys.chmod(0o600)

            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                port = listener.getsockname()[1]
            config = root / "sshd_config"
            config.write_text(
                f"Port {port}\nListenAddress 127.0.0.1\nHostKey {host_key}\n"
                f"PidFile {root / 'sshd.pid'}\nAuthorizedKeysFile {authorized_keys}\n"
                "StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n"
                "UsePAM no\nPermitRootLogin no\nLogLevel ERROR\n",
                encoding="utf-8",
            )
            sshd_path = shutil.which("sshd")
            sshd = subprocess.Popen([sshd_path, "-D", "-f", str(config), "-E", str(root / "sshd.log")])
            try:
                user = pwd.getpwuid(os.getuid()).pw_name
                ssh_dir = root / ".ssh"
                ssh_dir.mkdir()
                ssh_config = ssh_dir / "config"
                ssh_config.write_text(
                    "Host copy-rs-test\n  HostName 127.0.0.1\n"
                    f"  Port {port}\n  User {user}\n  IdentityFile {client_key}\n"
                    "  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n",
                    encoding="utf-8",
                )
                env = os.environ.copy()
                env["HOME"] = str(root)
                env["XDG_STATE_HOME"] = str(root / "state")
                env["COPY_RS_DISABLE_ETA_PRIORS"] = "1"
                env["RSYNC_RSH"] = f"ssh -F {ssh_config}"
                for _ in range(100):
                    ready = subprocess.run(
                        ["ssh", "-F", str(ssh_config), "copy-rs-test", "true"],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    if ready.returncode == 0:
                        break
                    time.sleep(0.02)
                else:
                    self.fail((root / "sshd.log").read_text(encoding="utf-8"))

                source = root / "source.txt"
                remote_dir = root / "remote"
                download_dir = root / "download"
                source.write_text("through ssh and rsync", encoding="utf-8")
                remote_dir.mkdir()
                download_dir.mkdir()
                upload = subprocess.run(
                    [str(COPY_BIN), str(source), f"copy-rs-test:{remote_dir}/"],
                    input="y\n", text=True, capture_output=True, env=env,
                )
                self.assertEqual(upload.returncode, 0, upload.stdout + upload.stderr)
                self.assertEqual((remote_dir / source.name).read_text(encoding="utf-8"), "through ssh and rsync")

                download = subprocess.run(
                    [str(COPY_BIN), f"copy-rs-test:{remote_dir / source.name}", str(download_dir)],
                    input="y\n", text=True, capture_output=True, env=env,
                )
                self.assertEqual(download.returncode, 0, download.stdout + download.stderr)
                self.assertEqual((download_dir / source.name).read_text(encoding="utf-8"), "through ssh and rsync")
            finally:
                sshd.terminate()
                sshd.wait(timeout=5)


if __name__ == "__main__":
    unittest.main(verbosity=2)
