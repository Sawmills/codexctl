"""Exercise the checked-in Job script on a disposable PostgreSQL schema.

Run with B33_TEST_DATABASE_URL pointing to local codexctl_a5 using an explicit role.
Requires psql and PyYAML; never uses the operator's default connection.
"""

import os
from pathlib import Path
import subprocess
import unittest
import uuid
from unittest import mock
from urllib.parse import urlparse

import yaml


class InventoryJobTest(unittest.TestCase):
    def test_rejects_routing_overrides_before_psql(self):
        for url in (
            "postgresql://postgres@localhost/codexctl_a5?hostaddr=192.0.2.1",
            "postgresql://localhost/codexctl_a5",
        ):
            with self.subTest(url=url), mock.patch.dict(os.environ, {"B33_TEST_DATABASE_URL": url}), mock.patch("subprocess.run") as run:
                with self.assertRaises(ValueError):
                    self.test_cap_boundary_and_empty_inventory()
                run.assert_not_called()

    def test_cap_boundary_and_empty_inventory(self):
        url = os.environ.get("B33_TEST_DATABASE_URL")
        if not url:
            self.skipTest("Set B33_TEST_DATABASE_URL to a disposable loopback database")
        target = urlparse(url)
        if (target.scheme not in ("postgres", "postgresql")
                or target.hostname not in ("localhost", "127.0.0.1")
                or not target.username or target.password
                or target.query or target.fragment or target.path != "/codexctl_a5"):
            raise ValueError("Use an explicit loopback role/codexctl_a5 URL without password, query or fragment")
        connection_env = {k: v for k, v in os.environ.items() if not k.startswith("PG")}
        connection_env.update(PGHOST=target.hostname, PGPORT=str(target.port or 5432),
                              PGUSER=target.username, PGDATABASE=target.path.lstrip("/"))
        schema = "b33_probe_" + uuid.uuid4().hex
        limited_role = "b33_limited_" + uuid.uuid4().hex[:16]
        manifest = yaml.safe_load(
            (Path(__file__).resolve().parents[1] / "deploy/k8s/jobs/b33-check-empty.yaml").read_text()
        )
        script = manifest["spec"]["template"]["spec"]["containers"][0]["args"][0]

        def sql(statement):
            subprocess.run(
                ["psql", "-X", "-v", "ON_ERROR_STOP=1", "-c", statement],
                env=connection_env, check=True, capture_output=True, text=True,
            )

        def inventory(database=None, user=None):
            env = dict(connection_env, PGDATABASE=database or connection_env["PGDATABASE"],
                       PGUSER=user or connection_env["PGUSER"],
                       PGOPTIONS=f"-c search_path={schema} -c default_transaction_read_only=on -c client_min_messages=warning")
            return subprocess.run(["sh", "-ec", script], env=env,
                                  capture_output=True, text=True, timeout=30)

        wrong_database = inventory("postgres")
        self.assertNotEqual(wrong_database.returncode, 0)
        self.assertIn("requires codexctl_a5", wrong_database.stderr)
        self.assertNotIn("inventory complete", wrong_database.stderr)
        sql(f"CREATE SCHEMA {schema}")
        rogue_schema = "b33_rogue_" + uuid.uuid4().hex
        try:
            sql(f"CREATE ROLE {limited_role} LOGIN; GRANT USAGE ON SCHEMA {schema} TO {limited_role}; "
                f"CREATE SCHEMA {rogue_schema}; CREATE TABLE {rogue_schema}.unexpected_rows (n integer); "
                f"REVOKE ALL ON SCHEMA {rogue_schema} FROM {limited_role}; "
                f"REVOKE ALL ON {rogue_schema}.unexpected_rows FROM {limited_role}")
            outside_allowed = inventory(user=limited_role)
            self.assertNotEqual(outside_allowed.returncode, 0, "inventory ignored a non-system schema")
            self.assertIn("outside allowed schema", outside_allowed.stderr)
            self.assertNotIn("inventory complete", outside_allowed.stderr)
            sql(f"DROP SCHEMA {rogue_schema} CASCADE")
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
            sql(f"DROP SCHEMA IF EXISTS {rogue_schema} CASCADE")
            sql(f"DROP SCHEMA {schema} CASCADE")
            sql(f"DROP ROLE IF EXISTS {limited_role}")


if __name__ == "__main__":
    unittest.main()
