"""Execute the manifest workflow step against a synthetic Buildx CLI contract."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/central-image.yml"
DIGEST = "sha256:" + "a" * 64
IMAGE = "registry.example.invalid/codexctl-central"
DOCKER = r'''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
if args[:3] == ["buildx", "imagetools", "create"]:
    assert args[3:] == ["--tag", os.environ["IMAGE"] + ":sha-" + os.environ["GITHUB_SHA"],
                       os.environ["IMAGE"] + "@sha256:" + "b" * 64,
                       os.environ["IMAGE"] + "@sha256:" + "c" * 64], args
elif args[:3] == ["buildx", "imagetools", "inspect"]:
    if os.environ.get("INSPECT_FAIL"):
        print("synthetic registry lookup failed", file=sys.stderr)
        sys.exit(1)
    assert args[3] == os.environ["IMAGE"] + ":sha-" + os.environ["GITHUB_SHA"]
    value = json.loads(os.environ["MANIFEST"])
    if args[4:] == ["--format", "{{.Manifest.Digest}}"]:
        # Buildx 0.32.1 routes every {{.Manifest... template to its human printer.
        print("Name: " + args[3] + "\nMediaType: application/vnd.oci.image.index.v1+json\nDigest: " + str(value.get("digest")))
    else:
        assert args[4:] == ["--format", "{{json .Manifest}}"], args
        print(json.dumps(value))
else:
    raise AssertionError(args)
'''


class CentralImageManifestTests(unittest.TestCase):
    def publish(self, manifest, fail=False):
        text = WORKFLOW.read_text()
        step = text.split("      - name: Publish manifest and deployment reference\n", 1)[1]
        body = textwrap.dedent(step.split("        run: |\n", 1)[1].split("      - uses:", 1)[0])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "bin"
            tools.mkdir()
            docker = tools / "docker"
            docker.write_text(DOCKER)
            docker.chmod(0o755)
            (root / "digests").mkdir()
            (root / "digests/amd64").write_text("sha256:" + "b" * 64 + "\n")
            (root / "digests/arm64").write_text("sha256:" + "c" * 64 + "\n")
            environment = {
                "PATH": str(tools) + os.pathsep + os.environ["PATH"],
                "IMAGE": IMAGE,
                "GITHUB_SHA": "d" * 40,
                "GITHUB_STEP_SUMMARY": str(root / "summary"),
                "MANIFEST": json.dumps(manifest),
            }
            if fail:
                environment["INSPECT_FAIL"] = "1"
            result = subprocess.run(["bash", "-c", body], cwd=root, env=environment,
                                    capture_output=True, text=True, timeout=10)
            artifact = root / "central-image.txt"
            return result, artifact.read_text() if artifact.exists() else None

    def test_buildx_032_manifest_contract_publishes_the_immutable_reference(self):
        result, artifact = self.publish({"digest": DIGEST, "schemaVersion": 2})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(artifact, IMAGE + "@" + DIGEST + "\n")

    def test_invalid_registry_digest_names_the_value_and_publishes_no_reference(self):
        result, artifact = self.publish({"digest": "sha256:not-valid"})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid central image digest", result.stderr)
        self.assertIn("sha256:not-valid", result.stderr)
        self.assertIsNone(artifact)

    def test_missing_registry_digest_is_visible_and_publishes_no_reference(self):
        result, artifact = self.publish({"schemaVersion": 2})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid central image digest", result.stderr)
        self.assertIn("null", result.stderr)
        self.assertIsNone(artifact)

    def test_failed_inspect_does_not_publish_a_deployment_reference(self):
        result, artifact = self.publish({}, fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("synthetic registry lookup failed", result.stderr)
        self.assertIsNone(artifact)


if __name__ == "__main__":
    unittest.main()
