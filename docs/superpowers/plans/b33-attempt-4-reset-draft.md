# B33 attempt-4 reset: guarded draft, not execution approval

Recommendation: request a fresh staging database from the infra owner and retain
the existing database unchanged. Backfill can write permanent identity claims
and alias tombstones. The available count report cannot identify those rows or
distinguish them from unrelated history. Erasing that evidence is unsafe.

The alternative Job, [b33-reset-data.yaml](../../../deploy/k8s/jobs/b33-reset-data.yaml),
is prepared only. It is suspended, excluded from every Kustomization, lacks the
approval marker, and has no confirmed staging hostname. It must not be created
or unsuspended. H and I do not include a reset.

## Evidence and its limits

The t56 report on [SAW-12484](https://linear.app/sawmills/issue/SAW-12484),
2026-10-12 18:17:44Z, says W merged at 17:21Z; migrate was a layout-8 no-op;
backfill reported accounts8/users4/devices10; H was refused before activation;
the file writer was restored at 18:16Z. A 15:36:30Z pre-window report says
18 tables, migrations8/progress5, and all other counts0.

The underlying receipts are
~/.codex/task-receipts/codexctl-staging/b33-a4-window-20261012/ and
b33-a4-prewindow-20261012 on t56's machine. They are absent on this devbox.
HQ has been asked to copy the table-name/count receipts and migrate/backfill
logs, without credentials. The summary is evidence of a reported outcome, not
independent row provenance.

The selected attempt-4 image
sha256:6374438eca220c90904c3d1623dc8fd91e18f65c27908fc9c828a0d55b74979e
comes from codexctl cff2988. Its schema declares 19 tables. The pre-window
18-table report needs a named inventory and post-migrate receipt; do not guess
which table was missing. The draft refuses a different inventory, including
new dashboard tables.

## What backfill writes

Source: CentralStore::backfill in src/central/storage.rs at the selected image,
PostgresStore::save_account, identity::record_claims,
identity::save_alias_tombstones, and PostgresStore::import_loans.

| Table                                      | Backfill path and attempt-4 evidence                                                                                 | Draft treatment                                    |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------- |
| central_accounts                           | Reported 8 copies. Existing revisions are skipped. Exact keys/revisions are not in the available report.             | Requires count8 after all other gates.             |
| central_users                              | Reported source inventory4; CAS may skip existing rows.                                                              | Requires count4 after all other gates.             |
| central_devices                            | Reported source inventory10; CAS may skip existing rows. Ordinary backfill does not retire relay limiters.           | Requires count10 after all other gates.            |
| central_account_claims                     | Account writes can insert UID/sub claims into the compatibility projection. No count or row receipt available.       | Any row refuses.                                   |
| central_account_identity_claims            | Account writes can insert the same keys into the independent permanent ledger. No count or row receipt available.    | Any row refuses.                                   |
| central_alias_tombstones                   | Copied from retained file-mode rename tombstones; omitted from the JSON counters. No count or row receipt available. | Any row refuses.                                   |
| account_loans                              | Copied from file loan state if present. Canary reported loans0; the live summary supplies no loan detail.            | Any row refuses.                                   |
| account_loan_audit                         | Copied with file loan audit, separately from the loan counter. No live row receipt available.                        | Any row refuses.                                   |
| account_refresh_leases                     | Ordinary account backfill does not acquire refresh leases. Released or expired rows still carry settlement evidence. | Any row refuses, including released.               |
| central_login_holders                      | Ordinary backfill does not start login workers. Expiry is not settlement proof.                                      | Any row refuses, including expired.                |
| central_login_operations                   | Ordinary backfill checks pending file logins but does not copy login journals into this table.                       | Any row refuses, including unresolved or terminal. |
| central_login_identity_reservations        | Ordinary backfill does not start a login or reserve a candidate identity.                                            | Any row refuses.                                   |
| central_login_identity_reservation_history | Ordinary backfill does not create reservation history. Archived rows remain proof.                                   | Any row refuses, including archived.               |
| account_live_sessions                      | Ordinary backfill does not deliver PostgreSQL tokens or record activity.                                             | Any row refuses.                                   |
| central_token_evidence                     | Ordinary backfill does not run the lease-path observation publisher.                                                 | Any row refuses.                                   |
| central_relay_rate_limits                  | Ordinary registry backfill does not call the event endpoint or revocation retirement.                                | Any row refuses.                                   |
| enrollment_challenges                      | Ordinary backfill does not copy browser/enrollment flow state.                                                       | Any row refuses.                                   |
| central_schema_migrations                  | Migrate retains/records versions1..8.                                                                                | Must match all8; preserved.                        |
| central_identity_migration_progress        | Migrate records completed claims/accounts/operations/reservations/source_fence stages.                               | Must match5 completed active stages; preserved.    |

No history-row exception is approved. This is deliberately stricter than
accepting every row that could have come from backfill: each exception would need
an independently reviewed row receipt, not just a count or timestamp. Legitimate
attempt-4 claim/tombstone rows therefore also stop this draft. That refusal is
why a fresh database is recommended.

Core tombstones, disabled users, revoked devices, and nonzero registry revisions
also refuse: these rows carry deletion or authorization fences even if the
counts still match. No exceptions for these core flags are approved.

The three core counts are consistency checks, not provenance or execution
authorization. They cannot by themselves enable the Job. The fixed approval
marker remains absent until HQ reviews target, backup, custody and provenance.

## Target binding and infra-owner open items

Non-secret source:

- infra/stacks/catalog/rds/codexctl.yaml: database_name and db_user are codexctl.
- infra/stacks/orgs/sawmills/plat/staging/us-east-1/rds.yaml: staging imports that component on the shared quota-manager RDS instance.
- infra/manifests/codexctl/external-secret.yaml: host remoteRef is /rds/quota-manager/codexctl/db_hostname; DB name remoteRef is /rds/quota-manager/codexctl/db_name.
- infra/components/terraform/rds/codexctl/main.tf: host is copied from the administrator hostname parameter; the exported parameters are SecureString. No literal endpoint appears in the inspected code.

No Secret or SecureString value was read. The concrete staging hostname remains
an infra-owner open item. Confirm the actual database name as well as its declared
value before a reviewed commit replaces the hostname placeholder.

The shell compares the actual secret-provided PGHOST and PGDATABASE to the
independent reviewed constants before invoking psql. Missing approval/host or a
different host/database makes zero DB calls. TLS verifies the hostname with the
RDS CA. No credential values are printed.

## Transaction and execution gates

The Job takes access-exclusive locks on the fixed inventory before checking
history/core counts and keeps them through TRUNCATE/COMMIT. Any history row,
core deletion/authorization fence,
incomplete migration, wrong layout, unexpected table or core count aborts without
changing data. No CASCADE is used. Metadata rows are retained. Lock wait is
bounded to5s, statements to60s, connection to5s, Job lifetime to120s.

Before even proposing an enabled revision: retain database and PVC backups,
obtain the missing receipts/target, stop every PostgreSQL writer and worker,
prove settlement, and get HQ/infra-owner decisions. If history provenance cannot
be established, preserve this DB and request a fresh one; do not weaken the guard
or erase ledger/journal rows. A draft review or merge grants no reset approval.

The real-PG fixture uses only a fresh disposable database on password-free
loopback PostgreSQL, with synthetic rows. It rejects URL routing parameters and
clears inherited libpq overrides for every client process. It executes the actual Job shell/SQL:
14 populated-history cases, target preflight refusals before DB contact, schema
and migration refusals, atomic preservation, and metadata preservation in an
empty-history fixture, plus seven core-fence cases. This synthetic schema does
not prove compatibility with the selected image's full migration DDL. Before
enabling any revision, the infra owner must also verify it against that exact
schema in a disposable database. No staging acceptance claim.
