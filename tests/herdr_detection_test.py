"""Exercise the real private-PTY launcher without credentials or provider calls."""

import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
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
    def run_resume(self, deleted_executable=False):
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
            executable = os.environ["CODEXCTL_TEST_BINARY"]
            descriptor = None
            if deleted_executable:
                # Execute the open inode after unlink: startup succeeds, but the
                # wrapper's current_exe path cannot be used for its hint exec.
                removed = home / "removed-codexctl"
                shutil.copy2(executable, removed)
                descriptor = os.open(removed, os.O_RDONLY)
                removed.unlink()
                executable = f"/proc/self/fd/{descriptor}"
            try:
                child = subprocess.Popen(
                    [executable, "codex", *args],
                    cwd=home, env=environment, stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    pass_fds=() if descriptor is None else (descriptor,),
                )
            finally:
                if descriptor is not None:
                    os.close(descriptor)
            stdout, stderr = child.communicate(timeout=20)
            self.assertEqual(child.returncode, 17, (stdout, stderr))
            observed = json.loads(receipt.read_text())
            self.assertTrue(observed["tty"])
            self.assertEqual(observed["parent"], child.pid, "keep the foreground PID")
            self.assertEqual(observed["args"], ["--cd", str(home), *args])
            self.assertEqual(observed["cwd"], str(home))
            return observed, stderr.decode()

    def test_private_pty_resume_exposes_the_foreground_wrapper_to_herdr(self):
        observed, _ = self.run_resume()
        self.assertTrue(observed["parent_hint"], "HerdR reads the foreground process environment, not the child environment")
        self.assertEqual(observed["child_hint"], "codex")

    @unittest.skipUnless(sys.platform == "linux", "open deleted executable via procfs")
    def test_failed_hint_exec_warns_and_continues_the_codex_launch(self):
        observed, stderr = self.run_resume(deleted_executable=True)
        self.assertFalse(observed["parent_hint"])
        self.assertIn("codexctl: herdr agent hint skipped:", stderr)
        self.assertIn("No such file or directory", stderr)


if __name__ == "__main__":
    unittest.main()
