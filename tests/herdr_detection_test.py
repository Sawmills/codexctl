"""Exercise the real private-PTY launcher without credentials or provider calls."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


FAKE_CODEX = r'''#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
parent = os.getppid()
if sys.platform == "linux":
    parent_hint = b"HERDR_AGENT=codex" in pathlib.Path(f"/proc/{parent}/environ").read_bytes().split(b"\0")
else:
    # The parent has only the synthetic environment supplied by this test.
    parent_hint = "HERDR_AGENT=codex" in subprocess.check_output(["/bin/ps", "eww", "-p", str(parent), "-o", "command="], text=True).split()
pathlib.Path(os.environ["FAKE_RECEIPT"]).write_text(json.dumps({
    "parent": parent, "parent_hint": parent_hint,
    "child_hint": os.environ.get("HERDR_AGENT"),
    "args": sys.argv[1:], "cwd": os.getcwd(), "tty": os.isatty(0)
}))
sys.exit(17)
'''


class HerdrDetectionTest(unittest.TestCase):
    def test_private_pty_resume_exposes_the_foreground_wrapper_to_herdr(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory).resolve()
            scripts = home / "bin"
            scripts.mkdir()
            codex = scripts / "codex"
            codex.write_text(FAKE_CODEX)
            codex.chmod(0o755)
            receipt = home / "receipt.json"
            environment = {
                "HOME": str(home),
                "PATH": str(scripts) + ":/usr/bin:/bin",
                "TERM": "xterm-256color",
                "FAKE_RECEIPT": str(receipt),
                "HERDR_ENV": "1",
                "HERDR_PANE_ID": "test:p1",
                "HERDR_SOCKET_PATH": str(home / "absent.sock"),
            }
            args = ["resume", "11111111-2222-4333-8444-555555555555", "--model", "test-model"]
            child = subprocess.Popen(
                [os.environ["CODEXCTL_TEST_BINARY"], "codex", *args],
                cwd=home, env=environment, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            )
            stdout, stderr = child.communicate(timeout=20)
            self.assertEqual(child.returncode, 17, (stdout, stderr))
            observed = json.loads(receipt.read_text())
            self.assertTrue(observed["tty"])
            self.assertEqual(observed["parent"], child.pid, "keep the foreground PID")
            self.assertEqual(observed["args"], ["--cd", str(home), *args])
            self.assertEqual(observed["cwd"], str(home))
            self.assertTrue(observed["parent_hint"], "HerdR reads the foreground process environment, not the child environment")
            self.assertEqual(observed["child_hint"], "codex")


if __name__ == "__main__":
    unittest.main()
