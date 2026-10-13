"""Run alert behavior tests against both real overlay rules with promtool."""
import pathlib
import shutil
import subprocess
import tempfile

import yaml

repo = pathlib.Path(__file__).resolve().parents[2]
with tempfile.TemporaryDirectory(prefix="codexctl-dashboard-alerts-") as temporary:
    for overlay in ("staging", "staging-ha"):
        directory = pathlib.Path(temporary) / overlay
        directory.mkdir()
        manifests = yaml.safe_load_all(
            (repo / "deploy/k8s/overlays" / overlay / "monitoring.yaml").read_text()
        )
        rule = next(doc["spec"] for doc in manifests if doc["kind"] == "PrometheusRule")
        (directory / "rules.yaml").write_text(yaml.safe_dump(rule))
        shutil.copy(repo / "tests/fixtures/dashboard-alerts.test.yaml", directory / "tests.yaml")
        subprocess.run(
            [
                "docker", "run", "--rm", "--entrypoint", "promtool",
                "-v", f"{directory}:/rules:ro", "-w", "/rules",
                "prom/prometheus:v3.15.0", "check", "rules", "rules.yaml",
            ],
            check=True,
        )
        subprocess.run(
            [
                "docker", "run", "--rm", "--entrypoint", "promtool",
                "-v", f"{directory}:/rules:ro", "-w", "/rules",
                "prom/prometheus:v3.15.0", "test", "rules", "tests.yaml",
            ],
            check=True,
        )
        print(f"{overlay} dashboard alert behavior PASS", flush=True)
