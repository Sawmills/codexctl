"""Exercise the checked-in Job script on a disposable PostgreSQL schema.

Run with B33_TEST_DATABASE_URL pointing to a disposable local database.
Requires psql and PyYAML; never uses the operator's default connection.
"""

import os
from pathlib import Path
import subprocess
import unittest
import uuid
from urllib.parse import urlparse

import yaml


class InventoryJobTest(unittest.TestCase):
    def test_cap_boundary_and_empty_inventory(self):
        url = os.environ.get("B33_TEST_DATABASE_URL")
        if not url:
            self.skipTest("Set B33_TEST_DATABASE_URL to a disposable loopback database")
        target = urlparse(url)
        if target.hostname not in ("localhost", "127.0.0.1") or target.password:
            raise ValueError("Use a disposable loopback PostgreSQL database without a password")
        schema = "b33_probe_" + uuid.uuid4().hex
        manifest = yaml.safe_load(
            (Path(__file__).resolve().parents[1] / "deploy/k8s/jobs/b33-check-empty.yaml").read_text()
        )
        script = manifest["spec"]["template"]["spec"]["containers"][0]["args"][0]

        def sql(statement):
            subprocess.run(
                ["psql", "-X", "-v", "ON_ERROR_STOP=1", url, "-c", statement],
                check=True, capture_output=True, text=True,
            )

        def inventory():
            env = dict(os.environ, PGHOST=target.hostname, PGPORT=str(target.port or 5432),
                       PGUSER=target.username or "postgres", PGDATABASE=target.path.lstrip("/"),
                       PGOPTIONS=f"-c search_path={schema} -c default_transaction_read_only=on -c client_min_messages=warning")
            return subprocess.run(["sh", "-ec", script], env=env,
                                  capture_output=True, text=True, timeout=30)

        sql(f"CREATE SCHEMA {schema}")
        try:
            empty = inventory()
            self.assertEqual(empty.returncode, 0, empty.stderr)
            self.assertNotIn("inventory table=", empty.stderr)
            self.assertIn("inventory complete tables=0", empty.stderr)
            sql(f"CREATE TABLE {schema}.synthetic_rows (n integer); "
                f"INSERT INTO {schema}.synthetic_rows SELECT generate_series(1,50000)")
            accepted = inventory()
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            self.assertIn("inventory table=synthetic_rows row_count=50000", accepted.stderr)
            self.assertIn("inventory complete tables=1", accepted.stderr)
            sql(f"INSERT INTO {schema}.synthetic_rows VALUES (50001)")
            refused = inventory()
            self.assertNotEqual(refused.returncode, 0, "oversized inventory Job succeeded")
            self.assertIn("exceeds 50000 rows", refused.stderr)
            self.assertNotIn("inventory complete", refused.stderr)
        finally:
            sql(f"DROP SCHEMA {schema} CASCADE")


if __name__ == "__main__":
    unittest.main()
