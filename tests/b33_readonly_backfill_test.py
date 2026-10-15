"""Check the manual backfill template and the rendered overlays, without a cluster.

Run: python3 tests/b33_readonly_backfill_test.py -v
Requires kubectl (local Kustomize only) and PyYAML.
"""

from pathlib import Path
import subprocess
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]


class ReadonlyBackfillJobTest(unittest.TestCase):
    def test_manual_successor_protects_source_and_is_absent_from_synced_overlays(self):
        job = yaml.safe_load(
            (ROOT / "deploy/k8s/jobs/final-backfill-readonly.yaml").read_text()
        )
        self.assertEqual(job["kind"], "Job")
        self.assertEqual(job["metadata"]["name"], "codexctl-final-backfill-readonly")
        marker = "codexctl.sawmills.ai/manual-only"
        self.assertEqual(job["metadata"]["annotations"][marker], "true")
        self.assertTrue(job["spec"]["suspend"], "manual execution requires a separate GO")
        pod = job["spec"]["template"]["spec"]
        self.assertEqual(pod["securityContext"]["fsGroupChangePolicy"], "OnRootMismatch")
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
