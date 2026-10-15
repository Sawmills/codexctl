"""Check the manual backfill template and the rendered overlays, without a cluster.

Run: python3 tests/b33_readonly_backfill_test.py -v
Requires kubectl (local Kustomize only), PyYAML, Docker and the cached pinned
BusyBox image from the Job. Container checks use only synthetic files, no network.
"""

from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]


class ReadonlyBackfillJobTest(unittest.TestCase):
    def test_init_copies_a_private_key_readable_by_the_backfill_user(self):
        job = yaml.safe_load(
            (ROOT / "deploy/k8s/jobs/final-backfill-readonly.yaml").read_text()
        )
        pod = job["spec"]["template"]["spec"]
        init = next(c for c in pod["initContainers"] if c["name"] == "prepare-secrets")
        backfill = next(c for c in pod["containers"] if c["name"] == "backfill")

        def identity(container):
            security = dict(pod["securityContext"], **container["securityContext"])
            return f"{security['runAsUser']}:{security['runAsGroup']}"

        def docker(*args):
            return subprocess.run(
                ["docker", "run", "--rm", "--pull", "never", "--network", "none",
                 "--read-only", "--entrypoint", "sh", *args],
                capture_output=True, text=True, timeout=20,
            )

        with tempfile.TemporaryDirectory(prefix="readonly-backfill-key-") as directory:
            root = Path(directory)
            root.chmod(0o755)
            projected = root / "projected"
            keys = root / "keys"
            projected.mkdir(mode=0o755)
            keys.mkdir()
            keys.chmod(0o777)  # Model kubelet's writable emptyDir, not the source PVC.
            # Model the Secret projection's root:root 0440 file without fsGroup.
            setup = docker(
                "--user", "0:0", "--volume", f"{projected}:/projected",
                init["image"], "-ec",
                "printf synthetic-key >/projected/vault-key; chmod 0440 /projected/vault-key",
            )
            self.assertEqual(setup.returncode, 0, setup.stderr)
            init_args = [
                "--cap-drop", "ALL",
                "--security-opt", "no-new-privileges",
                "--volume", f"{projected}:/projected:ro", "--volume", f"{keys}:/keys",
                init["image"], *init["command"][1:], *init["args"],
            ]
            denied = docker("--user", "10001:10001", *init_args)
            self.assertNotEqual(denied.returncode, 0, "projection must enforce private read access")
            self.assertIn("Permission denied", denied.stderr)
            (keys / "vault-key").unlink(missing_ok=True)
            copied = docker("--user", identity(init), *init_args)
            self.assertEqual(copied.returncode, 0, copied.stderr)
            readable = docker(
                "--user", identity(backfill), "--cap-drop", "ALL",
                "--security-opt", "no-new-privileges",
                "--volume", f"{keys}:/keys:ro", init["image"], "-ec",
                "cat /keys/vault-key >/dev/null; stat -c '%a %u %g' /keys/vault-key",
            )
            self.assertEqual(readable.returncode, 0, readable.stderr)
            self.assertEqual(readable.stdout.strip(), "600 10001 0")

    def test_manual_successor_protects_source_and_is_absent_from_synced_overlays(self):
        source = (ROOT / "deploy/k8s/jobs/final-backfill-readonly.yaml").read_text()
        # Render the standalone Job without adding it to a checked-in overlay.
        with tempfile.TemporaryDirectory(prefix="readonly-backfill-render-") as directory:
            root = Path(directory)
            (root / "job.yaml").write_text(source)
            (root / "kustomization.yaml").write_text("resources:\n  - job.yaml\n")
            rendered = subprocess.run(
                ["kubectl", "kustomize", str(root)],
                capture_output=True, text=True, check=True, timeout=30,
            )
        job = yaml.safe_load(rendered.stdout)
        self.assertEqual(job["kind"], "Job")
        self.assertEqual(job["metadata"]["name"], "codexctl-final-backfill-readonly")
        marker = "codexctl.sawmills.ai/manual-only"
        self.assertEqual(job["metadata"]["annotations"][marker], "true")
        self.assertTrue(job["spec"]["suspend"], "manual execution requires a separate GO")
        pod = job["spec"]["template"]["spec"]
        security = pod["securityContext"]
        self.assertNotIn("fsGroup", security, "kubelet must not change retained PVC ownership or modes")
        self.assertNotIn("fsGroupChangePolicy", security)
        self.assertEqual(security["runAsUser"], 10001)
        self.assertEqual(security["runAsGroup"], 10001)
        self.assertNotIn("supplementalGroups", security)
        containers = pod.get("initContainers", []) + pod["containers"]
        for container in containers:
            effective = dict(security, **container["securityContext"])
            self.assertEqual(effective["runAsUser"], 10001)
            self.assertTrue(effective["runAsNonRoot"])
            self.assertFalse(effective["allowPrivilegeEscalation"])
            self.assertEqual(effective["capabilities"].get("add", []), [])
            self.assertIn("ALL", effective["capabilities"]["drop"])
            expected_group = 0 if container["name"] == "prepare-secrets" else 10001
            self.assertEqual(effective["runAsGroup"], expected_group)
        init = next(c for c in pod["initContainers"] if c["name"] == "prepare-secrets")
        self.assertNotIn("state", [m["name"] for m in init["volumeMounts"]])
        projected = next(v for v in pod["volumes"] if v["name"] == "projected")
        self.assertEqual(projected["secret"]["defaultMode"], 0o440)
        backfill = next(c for c in pod["containers"] if c["name"] == "backfill")
        self.assertEqual(backfill["command"][:2], ["codexctl-central", "backfill"])
        state = next(m for m in backfill["volumeMounts"] if m["name"] == "state")
        self.assertEqual(state["mountPath"], "/data")
        self.assertTrue(state.get("readOnly"), "rollback evidence must not be writable")
        for container in pod.get("initContainers", []) + pod["containers"]:
            for mount in container.get("volumeMounts", []):
                if mount["name"] == "state":
                    self.assertTrue(mount.get("readOnly"))

        overlays = sorted((ROOT / "deploy/k8s/overlays").glob("*/kustomization.yaml"))
        self.assertTrue(overlays, "must exercise the actual synced overlays")
        for overlay in overlays:
            with self.subTest(overlay=overlay.parent.name):
                rendered = subprocess.run(
                    ["kubectl", "kustomize", str(overlay.parent)],
                    capture_output=True, text=True, check=True, timeout=30,
                )
                resources = list(yaml.safe_load_all(rendered.stdout))
                self.assertFalse(any(
                    r["kind"] == "Job" and (
                        "final-backfill-readonly" in r["metadata"].get("name", "")
                        or r["metadata"].get("annotations", {}).get(marker) == "true"
                    )
                    for r in resources
                ), "manual successor must never be synced by an overlay")
                if overlay.parent.name == "staging":
                    retained = next(r for r in resources if r["kind"] == "Job"
                                    and r["metadata"]["name"] == "codexctl-final-backfill-a5")
                    self.assertNotIn("ttlSecondsAfterFinished", retained["spec"])
                    self.assertNotIn("argocd.argoproj.io/hook", retained["metadata"].get("annotations", {}))


if __name__ == "__main__":
    unittest.main()
