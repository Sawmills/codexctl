"""Exercise rendered HA lifecycle hooks in isolated, synthetic PID namespaces.

Run: python3 tests/ha_lifecycle_test.py -v
Requires kubectl (local Kustomize only), PyYAML, Docker, and the cached Node image
below. No Kubernetes connection, live state, credentials, or network is used.
"""

import json
from pathlib import Path
import subprocess
import time
import unittest
import uuid

import yaml


ROOT = Path(__file__).resolve().parents[1]
NODE_IMAGE = "node@sha256:b26b04c123d9ff8ab646ceb18b9d75a1173acf64b9a401094b906d27b29338d4"


class HaLifecycleTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.overlays = {}
        for overlay in ("staging", "staging-ha"):
            rendered = subprocess.run(
                ["kubectl", "kustomize", str(ROOT / "deploy/k8s/overlays" / overlay)],
                capture_output=True, text=True, check=True, timeout=30,
            )
            cls.overlays[overlay] = list(yaml.safe_load_all(rendered.stdout))

    def test_three_ha_replicas_allow_one_disruption_and_no_surge(self):
        for overlay, resources in self.overlays.items():
            with self.subTest(overlay=overlay):
                deployment = next(r for r in resources if r["kind"] == "Deployment"
                                  and r["metadata"]["name"] == "codexctl-ha")
                spec = deployment["spec"]
                self.assertEqual(spec["replicas"], 3)
                self.assertEqual(spec.get("strategy"), {
                    "type": "RollingUpdate",
                    "rollingUpdate": {"maxSurge": 0, "maxUnavailable": 1},
                })
                template = spec["template"]
                self.assertEqual(template["metadata"]["annotations"]["karpenter.sh/do-not-disrupt"], "true")
                pdb = next(r for r in resources if r["kind"] == "PodDisruptionBudget"
                           and r["metadata"]["name"] == "codexctl-ha")["spec"]
                self.assertEqual(pdb["maxUnavailable"], 1)
                self.assertNotIn("minAvailable", pdb)
                labels = template["metadata"]["labels"]
                self.assertTrue(pdb["selector"]["matchLabels"].items() <= labels.items())
                pod = template["spec"]
                spread = pod["topologySpreadConstraints"]
                self.assertEqual({c["topologyKey"] for c in spread}, {
                    "topology.kubernetes.io/zone", "kubernetes.io/hostname",
                })
                for constraint in spread:
                    self.assertEqual(constraint["maxSkew"], 1)
                    self.assertEqual(constraint["whenUnsatisfiable"], "DoNotSchedule")
                    self.assertTrue(constraint["labelSelector"]["matchLabels"].items() <= labels.items())
                anti_affinity = pod["affinity"]["podAntiAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
                self.assertTrue(any(c["topologyKey"] == "kubernetes.io/hostname"
                                    and c["labelSelector"]["matchLabels"].items() <= labels.items()
                                    for c in anti_affinity))

    def test_initialize_uses_broker_database_and_ca_before_serving(self):
        for overlay, resources in self.overlays.items():
            with self.subTest(overlay=overlay):
                deployment = next(r for r in resources if r["kind"] == "Deployment"
                                  and r["metadata"]["name"] == "codexctl-ha")
                pod = deployment["spec"]["template"]["spec"]
                init = next(c for c in pod["initContainers"] if c["name"] == "initialize")
                broker = next(c for c in pod["containers"] if c["name"] == "broker")
                self.assertEqual(init["resources"], {
                    "requests": {"cpu": "100m", "memory": "64Mi"},
                    "limits": {"cpu": "1", "memory": "256Mi"},
                })
                self.assertEqual(init["image"], broker["image"])
                expected = {e["name"]: e for e in broker["env"]
                            if e["name"] == "CODEXCTL_CENTRAL_STORE"
                            or e["name"].startswith("DB_")
                            or e["name"] == "CODEXCTL_CENTRAL_DB_CA_FILE"}
                self.assertEqual({e["name"]: e for e in init["env"]}, expected)
                ca = next(m for m in broker["volumeMounts"] if m["name"] == "rds-ca")
                self.assertIn(ca, init["volumeMounts"])
                self.assertTrue(ca["readOnly"])
                self.assertEqual(init["command"], ["sh", "-ec"])
                manifest = ROOT / "deploy/k8s/overlays" / overlay / (
                    "ha-deployment.yaml" if overlay == "staging" else "deployment.yaml")
                source = yaml.safe_load(manifest.read_text())
                source_init = next(c for c in source["spec"]["template"]["spec"]["initContainers"]
                                   if c["name"] == "initialize")
                self.assertEqual(init["args"], source_init["args"])
                self.assertIn("fi; codexctl-central migrate --state /data/state --key-file /keys/vault-key",
                              init["args"][0])

    def test_hook_returns_before_kubelet_term_and_shutdown_settles(self):
        for overlay, resources in self.overlays.items():
            deployment = next(r for r in resources if r["kind"] == "Deployment"
                              and r["metadata"]["name"] == "codexctl-ha")
            pod = deployment["spec"]["template"]["spec"]
            broker = next(c for c in pod["containers"] if c["name"] == "broker")
            hook = broker["lifecycle"]["preStop"]["exec"]["command"]
            for settle_ms in (0, 250):
                with self.subTest(overlay=overlay, settle_ms=settle_ms):
                    name = "codexctl-hook-test-" + uuid.uuid4().hex

                    def docker(*args, check=True):
                        return subprocess.run(
                            ["docker", *args], capture_output=True, text=True,
                            check=check, timeout=15,
                        )

                    program = (
                        "process.on('SIGTERM',()=>setTimeout(()=>{"
                        "console.log('settled');process.exit(0)},"
                        + str(settle_ms)
                        + "));console.log('ready');setInterval(()=>{},1000)"
                    )
                    try:
                        security = pod["securityContext"]
                        docker("run", "-d", "--pull", "never", "--name", name,
                               "--network", "none", "--read-only", "--cap-drop", "ALL",
                               "--user", f"{security['runAsUser']}:{security['runAsGroup']}",
                               "--entrypoint", "node", NODE_IMAGE, "-e", program)
                        deadline = time.monotonic() + 5
                        while "ready" not in docker("logs", name).stdout:
                            self.assertLess(time.monotonic(), deadline, "PID 1 did not start")
                            time.sleep(0.05)
                        started = time.monotonic()
                        result = docker("exec", name, *hook, check=False)
                        elapsed = time.monotonic() - started
                        state = json.loads(docker("inspect", "--format", "{{json .State}}", name).stdout)
                        self.assertEqual(
                            result.returncode, 0,
                            f"preStop exit={result.returncode}, server exit={state['ExitCode']}, "
                            f"stderr={result.stderr.strip()}",
                        )
                        self.assertGreaterEqual(elapsed, 5, "endpoint-removal delay was skipped")
                        self.assertTrue(state["Running"], "preStop terminated PID 1 before kubelet TERM")
                        docker("kill", "--signal", "TERM", name)
                        self.assertEqual(docker("wait", name).stdout.strip(), "0")
                        self.assertIn("settled", docker("logs", name).stdout)
                    finally:
                        docker("rm", "-f", name, check=False)


if __name__ == "__main__":
    unittest.main()
