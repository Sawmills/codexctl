"""Protocol tests for the release packager; Darwin tools are synthetic."""

import hashlib
import os
import shutil
import sys
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/package-release.sh"


class ReleasePackageTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux" and shutil.which("cc") and shutil.which("objcopy"), "GNU build tools required")
    def test_linux_keeps_real_debug_lines_and_debuglink_and_packed_units(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = root / "target/x86_64-unknown-linux-gnu/release"
            release.mkdir(parents=True)
            source = root / "sample.c"
            source.write_text("int main(void) { return 0; }\n")
            subprocess.run(["cc", "-g", str(source), "-o", str(release / "codexctl")], check=True, capture_output=True)
            shutil.copy2(release / "codexctl", release / "codexctl-central")
            (release / "codexctl.dwp").write_bytes(b"packed compilation units")
            result = subprocess.run(["bash", str(SCRIPT), "v0.0.0", "x86_64-unknown-linux-gnu", "Linux_x86_64"], cwd=root, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            sections = subprocess.check_output(["objdump", "-h", str(release / "codexctl")], text=True)
            self.assertIn(".gnu_debuglink", sections)
            self.assertNotIn(".debug_info", sections)
            debug = subprocess.check_output(["objdump", "-h", str(release / "codexctl.debug")], text=True)
            self.assertIn(".debug_line", debug)
            with tarfile.open(root / "codexctl_v0.0.0_Linux_x86_64.debug.tar.gz") as archive:
                self.assertEqual(set(archive.getnames()), {"codexctl.debug", "codexctl-central.debug", "codexctl.dwp"})
                self.assertEqual(archive.extractfile("codexctl.dwp").read(), b"packed compilation units")
            self.assertEqual(subprocess.run([str(release / "codexctl")], check=False).returncode, 0)

    def test_darwin_publishes_matching_dsym_and_keeps_symbol_table(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "bin"
            tools.mkdir()
            release = root / "target/aarch64-apple-darwin/release"
            release.mkdir(parents=True)
            for name in ("codexctl", "codexctl-central"):
                (release / name).write_bytes(b"synthetic Mach-O")
                bundle = release / f"deps/{name}-0123456789abcdef.dSYM"
                dwarf = bundle / f"Contents/Resources/DWARF/{name}-0123456789abcdef"
                dwarf.parent.mkdir(parents=True)
                dwarf.write_bytes(b"synthetic DWARF")
                (release / f"{name}.dSYM").symlink_to(Path("deps") / bundle.name, target_is_directory=True)
            (tools / "strip").write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> strip.log\n')
            (tools / "dwarfdump").write_text(
                '#!/bin/sh\nshift\nfor file do\n'
                'printf "UUID: 12345678-1234-1234-1234-123456789ABC (arm64) %s\\n" "$file"\ndone\n'
            )
            for tool in tools.iterdir():
                tool.chmod(0o755)
            env = {**os.environ, "PATH": f"{tools}:{os.environ['PATH']}"}
            result = subprocess.run(["bash", str(SCRIPT), "v0.0.0", "aarch64-apple-darwin", "Darwin_arm64"],
                                    cwd=root, env=env, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual((root / "strip.log").read_text().count("-S "), 2)
            symbols = root / "codexctl_v0.0.0_Darwin_arm64.dSYM.zip"
            with zipfile.ZipFile(symbols) as archive:
                self.assertIn("codexctl.dSYM/Contents/Resources/DWARF/codexctl-0123456789abcdef", archive.namelist())
                self.assertIn("codexctl-central.dSYM/Contents/Resources/DWARF/codexctl-central-0123456789abcdef", archive.namelist())
            uuids = (root / "codexctl_v0.0.0_Darwin_arm64.uuid.txt").read_text()
            self.assertEqual(uuids.count("UUID:"), 4)
            self.assertEqual((root / f"{symbols.name}.sha256").read_text().split()[0],
                             hashlib.sha256(symbols.read_bytes()).hexdigest())
            with tarfile.open(root / "codexctl_v0.0.0_Darwin_arm64.tar.gz") as archive:
                self.assertEqual(set(archive.getnames()), {"codexctl", "codexctl-central"})

    def test_darwin_refuses_missing_debug_information(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = root / "target/aarch64-apple-darwin/release"
            release.mkdir(parents=True)
            for name in ("codexctl", "codexctl-central"):
                (release / name).write_bytes(b"no debug info")
            result = subprocess.run(["bash", str(SCRIPT), "v0.0.0", "aarch64-apple-darwin", "Darwin_arm64"],
                                    cwd=root, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root / "codexctl_v0.0.0_Darwin_arm64.tar.gz").exists())


if __name__ == "__main__":
    unittest.main()
