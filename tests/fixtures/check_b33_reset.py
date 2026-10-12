"""Exercise the reset Job entry point against a disposable local PostgreSQL DB."""
import argparse
import os
import pathlib
import subprocess
import sys
import tempfile
import urllib.parse
import uuid

import yaml

parser = argparse.ArgumentParser()
parser.add_argument("--manifest", type=pathlib.Path)
parser.add_argument("--sql-only", action="store_true")
args = parser.parse_args()
repo = pathlib.Path(__file__).resolve().parents[2]
manifest = args.manifest or repo / "deploy/k8s/jobs/b33-reset-data.yaml"
job = yaml.safe_load(manifest.read_text())
script = job["spec"]["template"]["spec"]["containers"][0]["args"][0]

admin_url = os.environ["DATABASE_URL"]
url = urllib.parse.urlsplit(admin_url)
if url.hostname not in ("localhost", "127.0.0.1", "::1") or url.password:
    raise RuntimeError("reset tests require a password-free loopback PostgreSQL URL")
parameters = urllib.parse.parse_qsl(url.query, keep_blank_values=True, strict_parsing=True)
if url.scheme not in ("postgres", "postgresql") or url.fragment or any(
    key != "sslmode" or value != "disable" for key, value in parameters
):
    raise RuntimeError("reset tests permit only sslmode=disable; no libpq routing options")
client_environment = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
client_environment["PGCONNECT_TIMEOUT"] = "2"
database = "b33_reset_test_" + uuid.uuid4().hex
test_url = urllib.parse.urlunsplit(
    (url.scheme, url.netloc, "/" + database, url.query, url.fragment)
)


def psql(sql, connection=test_url):
    result = subprocess.run(
        ["psql", "-X", "-At", "-v", "ON_ERROR_STOP=1", connection],
        input=sql,
        env=client_environment,
        text=True,
        capture_output=True,
        timeout=15,
    )
    if result.returncode:
        raise RuntimeError(result.stderr)
    return result.stdout.strip()


def run(environment):
    return subprocess.run(
        ["sh", "-ec", script],
        env=environment,
        text=True,
        capture_output=True,
        timeout=15,
    )


def require(condition, message):
    if not condition:
        raise AssertionError(message)


environment = client_environment.copy()
environment.update(
    PGHOST=url.hostname,
    PGPORT=str(url.port or 5432),
    PGDATABASE=database,
    PGUSER=url.username or "postgres",
    PGSSLMODE="disable",
    PGCONNECT_TIMEOUT="2",
    EXPECTED_STAGING_HOST=url.hostname,
    EXPECTED_STAGING_DATABASE=database,
    B33_RESET_APPROVAL="attempt4-provenance-reviewed",
)
if not args.sql_only:
    # Probe this fixture itself before any real CREATE/DROP DATABASE calls.
    with tempfile.TemporaryDirectory(prefix="b33-reset-routing-") as temporary:
        directory = pathlib.Path(temporary)
        trace = directory / "psql-called"
        client = directory / "psql"
        client.write_text(
            '#!/bin/sh\n'
            'if [ -n "${PGHOSTADDR:-}${PGSERVICE:-}${PGSERVICEFILE:-}${PGOPTIONS:-}" ]; then\n'
            '  printf unsafe >> "$B33_TEST_TRACE"\n'
            'else\n'
            '  printf clean >> "$B33_TEST_TRACE"\n'
            'fi\ncat >/dev/null\n'
        )
        client.chmod(0o700)
        probe = environment | {
            "PATH": str(directory) + os.pathsep + environment["PATH"],
            "B33_TEST_TRACE": str(trace),
        }
        invocation = [sys.executable, str(pathlib.Path(__file__).resolve()),
                      "--manifest", str(manifest), "--sql-only"]
        for parameter in ["hostaddr=203.0.113.1", "host=production.invalid",
                          "service=production", "options=-csearch_path=other"]:
            result = subprocess.run(
                invocation, env=probe | {
                    "DATABASE_URL": test_url + ("&" if url.query else "?") + parameter,
                },
                capture_output=True, text=True, timeout=15,
            )
            require(result.returncode != 0, "routing override must refuse")
            require(not trace.exists(), parameter + ": refuse before DB contact")
        subprocess.run(
            invocation, env=probe | {"PGHOSTADDR": "203.0.113.1",
                                    "PGSERVICE": "production",
                                    "PGSERVICEFILE": "/nonexistent",
                                    "PGOPTIONS": "-csearch_path=other"},
            capture_output=True, text=True, timeout=15,
        )
        require(trace.exists(), "sanitized fixture reaches the local protocol stub")
        require("unsafe" not in trace.read_text(), "all psql environments sanitized")
        print("PASS: fixture rejects URL routing and clears inherited libpq overrides")
    # A protocol stub records whether the entry point attempts any DB contact.
    with tempfile.TemporaryDirectory(prefix="b33-reset-preflight-") as temporary:
        directory = pathlib.Path(temporary)
        trace = directory / "psql-called"
        client = directory / "psql"
        client.write_text('#!/bin/sh\nprintf called > "$B33_TEST_TRACE"\ncat >/dev/null\n')
        client.chmod(0o700)
        preflight = environment | {
            "PATH": str(directory) + os.pathsep + environment["PATH"],
            "B33_TEST_TRACE": str(trace),
        }
        for case, changed in [
            ("missing approval", {"B33_RESET_APPROVAL": "not-approved"}),
            ("unknown hostname", {"EXPECTED_STAGING_HOST": "__INFRA_HOST_REQUIRED__"}),
            ("wrong hostname", {"PGHOST": "production.invalid"}),
            ("wrong database", {"PGDATABASE": "production"}),
        ]:
            result = run(preflight | changed)
            require(result.returncode != 0, case + " must refuse")
            require(not trace.exists(), case + " must refuse before DB contact")
        print("PASS: missing approval/target and wrong host/DB make zero DB calls")

protected = [
    "central_login_operations",
    "account_live_sessions",
    "account_loan_audit",
    "account_loans",
    "account_refresh_leases",
    "central_account_claims",
    "central_account_identity_claims",
    "central_alias_tombstones",
    "central_login_holders",
    "central_login_identity_reservation_history",
    "central_login_identity_reservations",
    "central_relay_rate_limits",
    "central_token_evidence",
    "enrollment_challenges",
]
core = {"central_accounts": 8, "central_users": 4, "central_devices": 10}

psql("CREATE DATABASE " + database, admin_url)
try:
    ddl = """
    CREATE TABLE central_schema_migrations(version integer PRIMARY KEY);
    INSERT INTO central_schema_migrations SELECT generate_series(1,8);
    CREATE TABLE central_identity_migration_progress(
      stage text PRIMARY KEY, completed boolean, deleted_at timestamptz);
    INSERT INTO central_identity_migration_progress(stage,completed)
      VALUES('claims',true),('accounts',true),('operations',true),
            ('reservations',true),('source_fence',true);
    """
    for table, count in core.items():
        columns = "id integer PRIMARY KEY, deleted_at timestamptz"
        if table == "central_users":
            columns += ", enabled boolean DEFAULT true, revision bigint DEFAULT 0"
        if table == "central_devices":
            columns += ", revoked boolean DEFAULT false, revision bigint DEFAULT 0"
        ddl += (
            "CREATE TABLE " + table + "(" + columns + ");"
            "INSERT INTO " + table + "(id) SELECT generate_series(1," + str(count) + ");"
        )
    for table in protected:
        columns = "id integer PRIMARY KEY"
        if table == "account_refresh_leases":
            columns += ", released boolean DEFAULT true"
        if table == "central_login_holders":
            columns += ", expires_at timestamptz DEFAULT '2000-01-01', deleted_at timestamptz"
        if table == "central_login_operations":
            columns += ", phase text DEFAULT 'unresolved'"
        if table == "central_login_identity_reservation_history":
            columns += ", deleted_at timestamptz DEFAULT '2000-01-01'"
        ddl += "CREATE TABLE " + table + "(" + columns + ");"
    ddl += """
    ALTER TABLE account_live_sessions
      ADD FOREIGN KEY(id) REFERENCES central_accounts(id);
    """
    psql(ddl)
    for table in protected:
        # Includes expired holders, released leases and archived/terminal history.
        psql("INSERT INTO " + table + "(id) VALUES(1)")
        result = run(environment)
        require(result.returncode != 0, table + ": unproven history must refuse")
        require(psql("SELECT count(*) FROM " + table) == "1", table + ": retain evidence")
        for name, count in core.items():
            require(psql("SELECT count(*) FROM " + name) == str(count), "retain core data")
        psql("DELETE FROM " + table)
    for prepare, undo, reason in [
        ("CREATE TABLE unexpected(id integer)", "DROP TABLE unexpected", "extra table"),
        (
            "UPDATE central_schema_migrations SET version=9 WHERE version=8",
            "UPDATE central_schema_migrations SET version=8 WHERE version=9",
            "new schema",
        ),
        (
            "UPDATE central_identity_migration_progress SET completed=false",
            "UPDATE central_identity_migration_progress SET completed=true",
            "incomplete migration",
        ),
        (
            "INSERT INTO central_accounts(id) VALUES(9)",
            "DELETE FROM central_accounts WHERE id=9",
            "unexpected core count",
        ),
    ]:
        psql(prepare)
        require(run(environment).returncode != 0, reason + ": must refuse")
        require(psql("SELECT count(*) FROM central_users") == "4", "atomic refusal")
        psql(undo)
    for table, attribute, fenced, cleared in [
        ("central_accounts", "deleted_at", "now()", "NULL"),
        ("central_users", "deleted_at", "now()", "NULL"),
        ("central_devices", "deleted_at", "now()", "NULL"),
        ("central_users", "enabled", "false", "true"),
        ("central_devices", "revoked", "true", "false"),
        ("central_users", "revision", "1", "0"),
        ("central_devices", "revision", "1", "0"),
    ]:
        psql("UPDATE " + table + " SET " + attribute + "=" + fenced + " WHERE id=1")
        before = psql("SELECT " + attribute + "::text FROM " + table + " WHERE id=1")
        require(run(environment).returncode != 0, table + "." + attribute + ": retain fence")
        require(psql("SELECT count(*) FROM central_accounts") == "8", "retain core data")
        require(
            psql("SELECT " + attribute + "::text FROM " + table + " WHERE id=1") == before,
            table + "." + attribute + ": preserve fenced row",
        )
        psql("UPDATE " + table + " SET " + attribute + "=" + cleared + " WHERE id=1")
    blocker = subprocess.Popen(
        ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", test_url],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=client_environment,
        text=True,
    )
    try:
        blocker.stdin.write(
            "BEGIN;\nLOCK TABLE central_login_operations IN ACCESS EXCLUSIVE MODE;\n"
            "SELECT 'lock_ready';\n"
        )
        blocker.stdin.flush()
        require(blocker.stdout.readline().strip() == "lock_ready", "concurrent lock ready")
        refused = run(environment)
        require(refused.returncode != 0, "concurrent writer must block reset")
        require("lock timeout" in refused.stderr, "real PostgreSQL lock timeout observed")
        require(psql("SELECT count(*) FROM central_users") == "4", "lock timeout preserves data")
    finally:
        blocker.stdin.close()
        blocker.wait(timeout=5)
        blocker.stdout.close()
        blocker.stderr.close()
    result = run(environment)
    require(result.returncode == 0, result.stderr)
    for table in list(core) + protected:
        require(psql("SELECT count(*) FROM " + table) == "0", table + ": clear safe fixture")
    require(psql("SELECT count(*) FROM central_schema_migrations") == "8", "retain versions")
    require(
        psql("SELECT count(*) FROM central_identity_migration_progress") == "5",
        "retain migration progress",
    )
    print("PASS: 14 history and 7 core-fence cases refuse atomically; metadata preserved")
finally:
    psql("DROP DATABASE " + database, admin_url)
