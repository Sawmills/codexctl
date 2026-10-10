# Account server high availability design

**Status:** Phase 3 build. PostgreSQL is authoritative when
`CODEXCTL_CENTRAL_STORE=postgres`; file mode remains the default and the live
staging overlay is unchanged. The HA overlay in
`deploy/k8s/overlays/staging-ha` is build-only until HQ authorizes cutover after
infra#1513 provisions the database and `codexctl-postgres` ExternalSecret.

The current live staging shape remains one StatefulSet replica with an encrypted
`ReadWriteOncePod` claim (`deploy/k8s/base/statefulset.yaml`). The phase-3 target
uses three independent pods, no broker PVC, per-account PostgreSQL fencing, and
readiness that reports database health. A pod-local vault key remains a secret;
credential payloads are encrypted before PostgreSQL insertion.

PostgreSQL mode opens a reconnecting client with bounded connect, statement, and
operation timeouts. Every token request reads the committed account revision;
only a holder of the per-account lease and fencing epoch may run a refresh child
or persist a rotated credential. Shared-store startup hydrates accounts without
launching refresh children. A token request takes and renews the database lease
before initialization, records initialization-time rotations, and stops the child
before releasing the lease. Failed initialization retains the lease until the
child stops and its journal is persisted. Background recovery uses the same
stop-and-persist rule. A definitive database rejection, such as a tombstone or
newer credential, fences the owner and releases the lease and shutdown permit.
Transient database errors retain the journal and retry, including during shutdown.
File-mode startup keeps its existing refresh owners.

Shared-store imports also renew their lease through verification and child exit.
An import retry reads the latest committed credential under that lease. Failed
verification settles and persists its rotation before another import can retry.
Import admission still requires a stable identity inventory across local
accounts. A busy or settling owner returns `refresh_in_progress` to imports,
including a different alias, instead of blocking the replica's token requests.
Retry the import after the pending work settles; existing accounts can continue
to request tokens. If a reimport cannot prepare its replacement after stopping
the previous owner, that account remains fenced. Retry the import after fixing
the preparation error; token traffic does not override the failed admission.

If a child never settles or its journal stays unreadable, the broker retains the
lease and fences that account. Graceful shutdown waits for this work. It can
exceed the Deployment's 60-second termination allowance; forced pod termination
can lose an unpublished rotation. Preserve the pod and its journal for recovery
when settlement errors persist. This change does not provide durable recovery
from forced pod loss during an unfinished refresh.

The HA Deployment allows up to five minutes for startup through `/health` before
liveness checks begin. This allowance covers database hydration and registry setup.

Holder IDs contain the pod hostname and a
random boot nonce. Registry authorization reads PostgreSQL on every request and
mutations use one entity per compare-and-swap revision, so a stale pod cannot
replace a concurrent revoke, enrollment, or re-enable. Dual mode is retained
only for explicit migration commands and still requires
`CODEXCTL_CENTRAL_DUAL_ACK=1`; the server refuses dual serving.

The PostgreSQL server supports shared add and renewal receipts, status, and
cancellation. The read-only dashboard supports company sign-in, sessions,
sign-out, and the company user's machine list across replicas. Machine
enrollment and reset listing/redemption remain unavailable in shared mode.
Keep those workflows on the single file-mode writer until their shared storage
is complete.
Machine rows are shared, but last-seen time and last-used alias reflect only
token deliveries handled by the responding pod. These activity fields can be
empty or differ across replicas. Shared activity storage is outside this upgrade.

For the dashboard session upgrade, run the existing migrate Job before rolling
the server image. Migration adds `browser_sessions` and its expiry index with
`IF NOT EXISTS`; it keeps schema version 8, so the previous image can still run.
Do not include this image in B33 attempt 4. Merge and deploy it only after HQ
confirms that attempt 4 is closed.

The dashboard stores only the session cookie's digest, the company-user ID,
signed-in time, and one-hour expiry. The existing encrypted enrollment table
holds one-time sign-in state for five minutes. Pending sign-ins share a
1,024-flow cap across replicas. Creating a new flow retires expired and consumed
challenge rows before checking that cap. Pending sign-ins use a separate
cached connection so anonymous requests cannot queue behind account admission.
The PKCE verifier is derived
from the vault key and state with HMAC-SHA256, and is never persisted. All pods
use the same vault key. Sign-out ends the session on every replica;
each request checks the current company-user status. A missing session table at
startup logs one structured `browser_sessions_unavailable` error. Dashboard
routes return 503 with that reason, counted in
`codexctl_central_failed_requests_total`. Token routes and readiness remain
available. Run migrate and roll the image to restore the dashboard.
The session-table probe runs only when SSO is configured. Token-only pods skip
it. If the probe itself fails, startup logs `browser_sessions_probe_failed` with
the database cause and exits. Restore database access before restarting the
pod; this error does not mean the session table is missing. Successful company
identity links emit `SSO_IDENTITY_LINKED` with identity digests only, after the
transaction commits. Refused or rolled-back links emit no success event.

Shared-store startup
also fences retained relogin operations that still need replacement verification
(`Promoted` or unfinished `Retiring`). Complete those operations on the file-mode
writer before migration; shared mode does not verify them through ordinary token
requests.

TLS is required by default. `DATABASE_URL` must include `sslmode=require`; set
`CODEXCTL_CENTRAL_DB_CA_FILE` to `/etc/codexctl/rds-ca/global-bundle.pem`.
Both staging overlays include the `codexctl-rds-ca` ConfigMap generated from the
[official AWS RDS global CA bundle](../deploy/k8s/rds-ca/README.md).
The HA broker and migration/backfill Jobs mount it read-only. Certificate and
hostname verification stay enabled; the image system bundle lacks the RDS root.
Complete the CA reconciliation gate in section 5 before submitting either Job.
The database role owns only `codexctl` and is non-superuser; the live
ExternalSecret is supplied by infra#1513.

Migration 4 adds add admission and unique identity claims. The mounted vault key
lets migration populate those claims from existing encrypted credentials. It
retains both UID and subject facts, including claims omitted by later tokens.
A conflict stops migration before readiness. Keep writers drained and preserve
the source and partial results for operator reconciliation before retrying.
Backfill copies file-mode rename tombstones into PostgreSQL. Every replica
refuses add or import under a retired alias.

Migration 5 adds logical release markers, reservation history, and typed
candidate UID and subject columns. It copies migration-4 claims into
`central_account_identity_claims`, an independent ownership ledger without an
account foreign key. An old account cascade cannot erase this proof. The old
claim table remains a compatibility projection. Both tables retain the full
workspace/namespace/claim key. Active reads require `deleted_at IS NULL`;
archiving a claim does not permit another account to take its historical key.
Alias tombstones remain active permanent fences. Completed reservations retain
release timestamps. A released full-key slot can be reused, while
`central_login_identity_reservation_history` keeps one row per claim and login
operation. Completion releases slots and history in the same transaction.
Migration copies surviving rows only; it cannot recover previously deleted
history. Drain migration-4 writers before migration and deploy this version to
all replicas before resuming writes. Mixed writer versions are not supported.

Schema 6 records the layout version in the schema transaction. Every binary
checks the highest stored version when it opens a PostgreSQL backend. It refuses
a newer version with `schema vN is newer than this binary; upgrade
codexctl-central`. The PR1 schema from #143 was never deployed anywhere, so no
pre-change PostgreSQL reader exists. Pending add receipts can therefore use a
nullable account ID in the shared journal. Later binaries must keep the version
gate; no flag bypasses it.

Identity backfill commits at most 32 source rows and 8 MiB of encrypted payload
per batch. It reads indexed high-water keys, decrypts outside admission, and
commits proof and the last key together. Each database batch has a two-second
limit; each migration invocation has a five-minute maintenance budget. Resume
with the same `migrate` command and vault key after a failed batch or elapsed
budget. Completed batches remain committed; the interrupted batch rolls back.

A hard guard accepts at most 5,000 source rows in total: retained legacy claims,
live accounts, all shared login operations, and identity-reservation slots. It
counts at most one row beyond the bound before backfill and records that accepted
inventory for restart. Above the bound, migration refuses with `identity
migration inventory exceeds 5000 source rows; keep writers drained`. This bound
covers PostgreSQL identity backfill, not the separate file-import command.

Drain every writer, including device login and refresh children, before migration.
A database fence refuses source writes while readiness is incomplete. The layout
version alone does not enable serving. A separate final readiness checkpoint
requires all four inventories to complete; cutover reads only those bounded
checkpoints under admission. The server refuses startup before that checkpoint.
Staging must record its source counts and elapsed maintenance time within these
bounds before resuming writers. These local limits do not establish production
throughput or approve the B33 cutover.

Add, renewal candidate publication, credential writes, and migration take one
transaction advisory lock for their PostgreSQL schema. Lock order starts with
that admission lock, then the operation or account rows and refresh lease as
needed. No native child or provider call runs inside admission. Fresh database
time fences writes after lock waits. A unique workspace/namespace/claim key
prevents duplicate identity ownership; operation reservations refuse a second
matching login with `relogin_reserved`. Add creates the unverified account and
transfers its reservation together, then uses the fenced renewal verifier.
Same-user landing keeps the existing alias and label. Completion publishes
credentials, revision, receipt, and reservation release together. The machine
must remain authorized at admission. A fresh add can repair a rejected add
when its retained identity agrees with the existing account. A timed-out
admission reads the settled transaction before verification, so a committed
account does not leave its operation stuck at the old sequence.

The `/metrics` endpoint reports process-wide
`codexctl_central_admission_lock_seconds_sum` and
`codexctl_central_admission_lock_seconds_count`. These count client-observed
transaction hold time after acquiring the lock, including failed attempts.
They exclude connection and advisory-lock waits. A timed-out commit can keep
running after the client observation ends. Use the sum/count rates and the
existing staging workload gates to measure admission cost before a lock or
connection-pool redesign. Local tests verify that stalled native verification
does not hold this lock; they do not establish production throughput.

PostgreSQL schema v7 registers a unique holder for each replica boot. Its
30-second lease renews every five seconds and cannot renew after expiry. An
expired holder disables new shared login work. Operation writes and
refresh-capable verification also require that holder's live lease.

After authorization, status and start requests recover retained login receipts.
On Linux, an independent polling supervisor receives a heartbeat only after
its parent confirms operation authority. Pipe closure or ten seconds without a
heartbeat stops and awaits the native child. The child is also bound to the
supervisor's death. The supervisor owns both challenge and candidate publication. It reads durable
cancellation every second without renewing parent authority. It publishes the
captured grant before normal completion, or a durable absence receipt after
confirmed exit with no grant in an intact private home. A failed native spawn
also records absence only when that original home contains no grant. If the
native child exits before its process identity is recorded, the supervisor
reaps it and settles its grant evidence. The supervisor keeps the server working
directory for relative database CA paths; its native child runs in the private
login home. A failed settlement receipt read retains an unresolved fence and
the local grant evidence.
An expired polling operation and holder can release reservations only with
that absence receipt. The old receipt reports `expired`; a fresh request ID
can complete on another replica without first polling the abandoned ID.
Another authorized machine can start the fresh request while the original
machine retains exclusive access to its old receipt.

Lease expiry and parent-death capability alone never prove grant absence. If
both parent and supervisor die before a durable receipt, or grant publication
fails, retain the company-user fence and private home for operator settlement.
Missing holder records, retired holders, and unsupported holders cannot supply
automatic cleanup proof. Never inspect a foreign PID to infer exit.

A durable candidate can transfer to a new holder and epoch before verification
starts. The shared candidate remains authoritative if its local execution home
is lost after publication. Recovery preserves the candidate, identity reservations,
and account lease fences, then resumes admission and verification without another device
login. An unreleased foreign refresh lease still requires explicit settlement.
If the old login worker already acquired the account lease, its pre-verification
settlement can be uncertain; recovery reports `relogin_settlement_unresolved`
and keeps that evidence. A durable in-flight verification marker reports
`relogin_verification_unresolved`; no replica repeats verification.

A durable `failure_reported` marker gives each failed login one reporting
replica. The live holder reports its own failure; after holder loss a serving
replica claims the marker. Known failed grants count under `relogin_failed` and
unidentified evidence under `relogin_identity_unresolved`, with bounded
settlement-stage logs. Repeated receipt reads do not count another failure. The existing staging
`CodexctlCredentialOperationFailed` alert selects that reason for 300 seconds
after its failure timestamp. Local tests prove failure counts and fences;
notification delivery and live takeover acceptance require a separate rollout.

An unidentified grant still fences its company user's accounts. Use the operator
resolution steps in [the server guide](central-server.md) and preserve its
private execution home. Linux supplies a parent-death signal; macOS process
groups do not supply that signal or verified forced-parent-death recovery.

For an upgrade from a PostgreSQL schema without the `released` lease marker,
migration records a `legacy_handoff` marker on the old lease rows. It leaves
`released=false`. An expired old row is not proof of settlement. Before starting
new replicas, stop every old refresh owner, confirm child exit, reconcile its
credential journal to PostgreSQL, and confirm that no provider request has an
unknown result. Preserve those receipts. If settlement is unknown, retain the
fence and stop the upgrade.

After those checks, run the migration handoff with the existing private database
and vault-key environment:

```sh
codexctl-central migrate --state /data/state --key-file /keys/vault-key \
  --confirm-legacy-owners-settled
```

Retain its `legacyLeasesReleased` JSON receipt before starting replicas. The flag
releases only rows marked by the legacy migration. Every new acquisition or
normal release consumes that marker, so repeating this command cannot release a
current refresh owner. Ordinary `migrate` does not assert settlement or release
these leases. This procedure does not resolve unfinished login verification.

## 1. State inventory

The following inventory distinguishes durable source state from caches and
process-local state. The paths are the current file implementation; the target
database mapping is described in section 3.

| State                                            | Current evidence and behavior                                                                                                                                                                                                                                                                                                                                                                           | Access pattern                                                                                                                                                                                                                                                         | HA treatment                                                                                                                                                                                                                                                                          |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Vault files                                      | A vault stores alias, company user, metadata, and the full auth object (`src/central/vault.rs:16-28`). It is encrypted and atomically replaced (`src/central/vault.rs:155-174`). Each managed account is under `state/accounts/<account-key>` (`src/central/managed.rs:478-485`).                                                                                                                       | Read on startup, recovery, account lookup, and token snapshots; written after imports, refresh-token rotation, login renewal, and recovery (`src/central/managed.rs:165-185`). This is credential state and is written per request when a refresh rotates credentials. | Store encrypted credential blobs and a monotonically increasing revision in PostgreSQL. The encryption key remains in the secret store; it is not put in the database.                                                                                                                |
| Managed Codex homes                              | The refresh owner uses `accounts/<key>/runtime/auth.json`, `pid`, and `spawn-failed` (`src/central/managed.rs:892-924`, `src/central/managed.rs:937-966`). Login workers use `relogin/<operation>/home/auth.json` and a private `CODEX_HOME` (`src/central/relogin/worker.rs:76-98`).                                                                                                                   | `auth.json` is the file-backed journal and can change during a provider call; PID and spawn markers are process evidence. These are written by the refresh owner or login worker, not by ordinary reads.                                                               | Make the durable journal and process-independent login record database rows. A refresh owner may materialize a pod-local Codex home, but a follower must never start a second native owner for the same account. PID evidence is pod-local and is replaced by lease/fencing evidence. |
| User/account catalog                             | `users.json` is the company-user registry and is read by authorization (`src/central/managed.rs:138-142`, `263-283`). User enable/disable and first enrollment rewrite it under `users.lock` (`src/central/managed.rs:143-173`). Account metadata and aliases are held in `AccountIndex`/`Owner` and loaded from every `accounts/` directory at startup (`src/central/managed.rs:77-105`, `1018-1064`). | Read on every authenticated request; writes occur for enrollment, enable/disable, import, label changes, and account replacement.                                                                                                                                      | Normalize users, account identity, alias, labels, and status into transactional tables. Keep a unique `(company_user, account_identity)` constraint so two pods cannot create one account twice.                                                                                      |
| Device tokens and registry                       | Device records contain tenant, company user, token hash, and revocation (`src/central/vault.rs:30-37`). Authorization reads `devices.json` and `users.json` for every request (`src/central/managed.rs:263-283`). Enrollment and revoke write under `devices.lock` (`src/central/enrollment.rs:476-488`; `src/central/managed.rs:788-805`).                                                             | Read-mostly, with per-enrollment and per-revocation writes.                                                                                                                                                                                                            | Use `devices` rows with unique device IDs and a revoked flag. Hash the bearer token as today; never store the bearer value. Authorization reads committed rows on every request.                                                                                                      |
| Reset requests                                   | Each account has `reset-redemptions.json` with one pending operation and completed request receipts (`src/central/resets.rs:287-299`). The pending credit and idempotency key are persisted before the provider call and the result is persisted afterward (`src/central/resets.rs:357-441`).                                                                                                           | Read and write for every redemption, including retries from another machine; list is read-mostly and does not mutate the journal (`src/central/resets.rs:82-177`).                                                                                                     | Use `reset_operations` and `reset_operation_requests` rows in the same account transaction as the lease. Preserve the pending row until the provider result is resolved; unique request IDs make retries idempotent.                                                                  |
| Login-renewal (relogin) state                    | A renewal is a durable `relogin/<id>/record.json` with phase, process evidence, candidate auth, and error (`src/central/relogin/state.rs:23-50`, `67-103`). Candidate and reservation inventory intentionally retains unreadable evidence (`src/central/relogin/inventory.rs:65-134`).                                                                                                                  | Written at every phase transition and on child/process evidence; read at startup, recovery, status, cancellation, and identity-conflict checks.                                                                                                                        | Store the operation record and candidate encrypted auth in PostgreSQL. Keep the browser/device flow and native child pod-local until the operation is promoted; acquire the account lease before stopping or replacing its refresh owner.                                             |
| Enrollment state                                 | Pending device challenges, OIDC login state, and approval tokens are in three in-memory maps (`src/central/enrollment.rs:65-78`). They expire during cleanup (`src/central/enrollment.rs:136-140`) and are consumed once (`src/central/enrollment.rs:237-266`, `451-490`).                                                                                                                              | Read/write per browser or polling request; currently lost when a pod dies.                                                                                                                                                                                             | Put short-lived challenges, PKCE state, and approvals in PostgreSQL (or a shared expiring key store) with TTL and one-time-consume transactions. This is required for a poll or callback to land on another pod.                                                                      |
| Usage/catalog observations                       | The account catalog is a 60-second per-pod cache keyed by account and credential revision (`src/central/catalog.rs:11-25`, `69-105`). Listing copies only an access token and never refreshes or persists credentials (`src/central/managed.rs:375-442`).                                                                                                                                               | Read-mostly; cache fills on expiry and may issue one read-only upstream usage request.                                                                                                                                                                                 | Keep this cache local to each pod. It is derived state, so a pod may return stale/unknown usage and refill independently. Never use it as an ownership or refresh lock.                                                                                                               |
| SSO configuration, vault key, metrics credential | The SSO client secret is read from a private file at startup (`src/central/enrollment.rs:80-115`); the vault key and metrics token are projected secrets in staging (`deploy/k8s/base/statefulset.yaml` and the HA overlay secret mounts).                                                                                                                                                              | Read-only after startup.                                                                                                                                                                                                                                               | Continue using External Secrets/secret mounts. Rotate the vault key only with a coordinated re-encryption migration, as already documented (`docs/central-server.md:392-400`).                                                                                                        |
| Failure counters and shutdown flags              | Failure counters, ownership-unresolved, stopping, and the work semaphore are process memory (`src/central/managed.rs:90-105`, `243-261`).                                                                                                                                                                                                                                                               | Updated per request or lifecycle event.                                                                                                                                                                                                                                | Treat as pod-local telemetry. Export counters to Prometheus; do not use them as durable business state.                                                                                                                                                                               |

## 2. Refresh-token rotation and the single writer

OpenAI refresh-token rotation is destructive: a successful refresh can invalidate
the previous refresh token, so two pods must never call `account/read` with
`refreshToken=true` for one account at the same time. The current broker already
serializes calls for one `Owner` mutex and persists the complete auth revision
after every call (`src/central/server.rs:200-237`; `src/central/server.rs:257-264`).
That mutex cannot coordinate separate pods.

The target uses a **PostgreSQL per-account lease with a fencing epoch**:

1. `account_refresh_leases(account_id, holder_id, epoch, expires_at)` has one row
   per account. Acquisition is a transaction that succeeds only when the row is
   expired or already held by the caller; every acquisition increments `epoch`.
2. The holder renews the row periodically. Every credential write includes
   `WHERE account_id = $1 AND holder_id = $2 AND epoch = $3 AND expires_at > now()`.
   A paused or partitioned pod therefore cannot commit after a successor takes
   the lease. The write transaction updates the encrypted auth blob, access-token
   metadata, and credential revision together.
3. A token request first reads the committed credential revision and access token.
   If the cached access token is usable, any pod can return it. If a refresh is
   required, the pod tries to acquire the account lease. A follower that loses
   the race waits for the revision to change, then reads the new token; it never
   calls OpenAI itself. If the lease expires without a new revision, it retries
   acquisition until the request deadline and returns `owner_unavailable`.
4. The refresh owner is the only process allowed to run the native Codex refresh
   child for that account. It may use a pod-local `CODEX_HOME`; followers only
   read encrypted rows. The owner persists a rotated refresh token even when the
   caller disconnects, matching the current detached-task behavior
   (`src/central/server.rs:491-514`; `src/central/resets.rs:214-245`).
5. Login renewal, import verification, and reset redemption take the same
   account lease (or a transactionally compatible operation lock) before stopping
   or changing the refresh owner. This prevents a renewal or reset from racing a
   refresh and preserves the existing identity and idempotency fences.

### Valid-token fast path (layout 8)

Item 3 ships behind `CODEXCTL_CENTRAL_TOKEN_FAST_PATH=1`, in PostgreSQL mode
only; dual and file mode keep the lease path. Without the flag, every token
request takes the lease and starts a native child, as before. With it, any
replica serves the committed token without the lease or a child when all of
these hold:

- The access token expires at least one hour after the database clock, which
  leaves early-refresh time during an OpenAI auth outage.
- The request is not a forced refresh: `previousRevision` is absent or names an
  older revision.
- `central_token_evidence` holds evidence for this exact auth digest and integer
  account revision, observed at most 60 s ago, with a completed routing check.
- A `billing` request has billing evidence of the same age. A rate-limited
  account is served only while every window is below 90% used; otherwise the
  request takes a live read. Usage-based and unknown classes may be served,
  because the client refuses them.
- The row is not tombstoned, carries the requested user and alias, matches a
  requested `accountId`, is verified, and no login journal or identity
  reservation fences it (the same predicate as the lease claim). A locally
  fenced owner also takes the lease path. A busy local owner hides its fence,
  so the request waits for the owner lock and re-checks there. Any failed
  lease-path read tombstones the account's evidence, so no replica serves it until the lease path
  observes the account again. If that update fails three times, the failure is
  counted as `token_evidence_clear_failed`, and the old evidence still expires
  within 60 s.

The lease path publishes the evidence in the same `fenced_write` transaction as
the credential, under the valid-lease predicate, also when the credential is
unchanged. Its observation time is the database clock minus the time since the
child read. A request that meets another replica's live lease drops its owner
lock, import guard, and work permit, then polls the account revision with
jittered backoff (50 ms to 1 s) for up to 15 s: a new revision is served through
the fast path, a released lease is taken, and a timeout returns
`refresh_in_progress`.

`/metrics` reports `codexctl_central_token_fast_path_total{result}` for every
decision and `codexctl_central_lease_waits_total{outcome="committed|acquired|timeout|refused"}`
with `codexctl_central_lease_wait_seconds_sum` and `_count`. The staging
PrometheusRule alerts on lease-wait timeouts and on the `owner_unavailable`
rate; both carry `severity: warning` and route to the `warnings-slack` receiver.

Layout 8 adds `central_token_evidence`. A layout-7 binary refuses to start on a
layout-8 database, so roll every replica onto the layout-8 image before turning
the flag on; the flag only takes effect where every writer publishes evidence.

Lease parameters should be measured in staging; an initial proposal is a 15 s
lease, renewal every 5 s, and a 55 s request deadline. The 60 s outage objective
then leaves time for one failed pod, lease expiry, new acquisition, and a retry.
The lease epoch and credential revision must be logged without credential values.

## 3. Storage options

| Option                                                    | Cost and effort                                                                                                                                                                                                                                                                                                                                                                      | Risk                                                                                                                                                                                                                                                                                 | Assessment                                                                                                                                                                                                                            |
| --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **(a) Shared RWX volume (EFS) plus file locks**           | Lowest application migration: retain JSON files and add an EFS CSI class. Ongoing EFS throughput/request charges and cross-AZ data transfer. Test and tune every lock and atomic-rename path on NFS.                                                                                                                                                                                 | POSIX locks and the current `flock`-style guard are not a fencing protocol for a paused pod. NFS visibility and rename latency make journal recovery harder; a stale process can still refresh after another pod starts. Enrollment maps remain in memory unless separately changed. | Does not prove the refresh safety property. It also keeps credentials and process evidence coupled to filesystem semantics. Reject as the target.                                                                                     |
| **(b) PostgreSQL (the Sawmills RDS pattern)**             | Highest initial effort: schema, encrypted-row codec, migration/export tooling, connection pool, and lease tests. Uses the existing Sawmills pattern of encrypted private RDS for durable state and versioned migration jobs (`/home/amir/Code/agent-platform/infra/README.md:27-43`, `62-69`, `96-99`). RDS Multi-AZ and backups add managed cost but avoid a new storage subsystem. | Requires careful transaction boundaries, least-privilege roles, connection-pool sizing, and a compatibility release during migration. A database outage affects token reads, so retain a short-lived access-token cache and alert on DB health.                                      | **Recommend.** Transactions give atomic credential/revision updates, expiring leases provide fencing, and rows make followers’ reads deterministic. It also persists enrollment and reset operations instead of silently losing them. |
| **(c) Leader-only writes with followers proxying writes** | Medium code effort if one pod owns the existing PVC and followers forward mutations over the Service. Lowest schema work, but still needs a replicated/shared store or a fast RWO handoff for failover.                                                                                                                                                                              | The leader is a single bottleneck and a failed node can still produce the same Multi-Attach outage. Followers cannot safely answer reads while the leader is unavailable unless state is replicated. Proxy retries can duplicate refreshes unless the leader itself has fencing.     | Useful as a temporary compatibility mode during migration, not a final HA design.                                                                                                                                                     |

PostgreSQL is a deliberate, hard-to-reverse storage choice; ADR 0002 records it.

## 4. Kubernetes shape

The target workload is a Deployment (or a StatefulSet without per-pod storage)
with **three replicas** in staging and production. Two is the minimum for a
node failure; three gives the PDB and zone spread room during a voluntary
disruption. The Service remains ClusterIP, as today (`docs/central-server.md:380-385`).

- Require `topologySpreadConstraints` on `topology.kubernetes.io/zone` and
  `kubernetes.io/hostname`, `maxSkew: 1`, `whenUnsatisfiable: DoNotSchedule`,
  and the broker label as the selector. Add required pod anti-affinity on the
  hostname so a scheduler cannot co-locate all replicas on one node.
- Add a `PodDisruptionBudget` with `minAvailable: 1`. It protects one serving
  pod during a drain while allowing a controlled handoff.
- Keep `/health` as process liveness. Make `/ready` verify database reachability,
  readable catalog/device rows, and the serving mode. A follower is ready for
  read-only/token-cache traffic. If an endpoint is configured to require the
  refresh lease (for example, the compatibility write path), that pod must
  return 503 until it holds the lease; it must never advertise readiness while
  accepting an unsafe write. The current probes and semantics are
  `deploy/k8s/base/statefulset.yaml` and the probes in `deploy/k8s/overlays/staging-ha/deployment.yaml`.
- Use a short `preStop` drain: stop accepting new refresh acquisitions, finish
  in-flight persistence, release the account lease, and then terminate. A pod
  that loses the lease or database connection must fail readiness immediately.
- Remove the per-replica RWO claim from the broker. If a temporary compatibility
  release still mounts the old PVC, run exactly one writer and keep followers
  read-only; do not attach the RWO claim to multiple pods.

## 5. Staging migration and rollback

For the staging attempt 3, follow [the B33 attempt 3 runbook](superpowers/plans/b33-attempt-3-cutover.md): it delivers these steps through GitOps PRs and a Claude operator that uses no codexctl token.

The commands below require operator execution approval. Stop if the database,
role, secret, or row counts do not match the reviewed plan.

Before stopping the writer or submitting a Job, merge the reviewed CA change into
`main` and reconcile Application `codexctl` using `deploy/k8s/overlays/staging`.
Keep existing automated sync enabled and wait for that merge to sync. For an
installation using the checked-in manual-sync Application, run the approved
`argocd app sync codexctl`, then `argocd app wait codexctl --sync`.
Do not apply the HA overlay to satisfy this prerequisite.

Verify the synced revision is the reviewed CA merge (or a reviewed descendant
that includes it), and verify the bundle key in namespace `codexctl`:

```sh
kubie exec plat-staging argocd kubectl get application codexctl -o json | \
  jq '{source: .spec.source, sync: .status.sync}'
kubie exec plat-staging codexctl kubectl get configmap codexctl-rds-ca -o json | \
  jq -e '.data["global-bundle.pem"] | startswith("-----BEGIN CERTIFICATE-----")'
```

Stop if the revision is wrong, sync is incomplete, or the ConfigMap check fails.

1. **Back up the current PVC.** Keep the single live pod serving file mode.
   Freeze administrative mutations, take the filesystem copy at one point in
   time, and hash the exact archive kept for recovery before changing its
   environment:

   ```sh
   kubectl --context plat-staging -n codexctl cp codexctl-0:/data/state ./codexctl-state-backup
   tar -C ./codexctl-state-backup -czf ./codexctl-state-backup.tar.gz .
   sha256sum ./codexctl-state-backup.tar.gz
   ```

   Keep `codexctl-state-backup.tar.gz` and its checksum together. Restore from
   that archive, not from a second live copy.

2. **Provision and migrate.** After infra#1513 is merged and applied, stop the
   file-mode writer and wait for `codexctl-0` to terminate before attaching its
   ReadWriteOncePod claim to a migration Job. Then wait for
   `externalsecret/codexctl-postgres` to be Ready. Run the explicit schema
   migration and backfill from the retained PVC using the projected vault key;
   retain the JSON count output as the migration receipt:

   Use one-shot migration Jobs with `secretKeyRef` inputs. Do not pass the
   database password through `kubectl exec` arguments or shell expansion. The
   migration Job mounts the retained PVC at `/data` and runs `migrate`; submit
   a second copy with `backfill` as its command after migration completes:

   Apply the migration-only egress policy before creating either Job. It
   matches the `app.kubernetes.io/name: codexctl-migration` label below and
   permits only cluster DNS and the private PostgreSQL network:

   ```sh
   kubectl --context plat-staging apply -f deploy/k8s/overlays/staging-ha/migration-networkpolicy.yaml
   ```

   The scale-down starts a maintenance window: the existing Ingress has no
   token-serving backend until the HA Deployment is ready. Announce the
   expected token outage, reject or drain token traffic during migration, and
   do not scale down outside that window. Apply the HA overlay and switch the
   Ingress only after its three pods pass the checks in step 4.

   ```sh
   kubectl --context plat-staging -n codexctl scale statefulset/codexctl --replicas=0
   kubectl --context plat-staging -n codexctl wait --for=delete pod/codexctl-0 --timeout=120s
   kubectl --context plat-staging -n codexctl wait --for=condition=Ready \
     externalsecret/codexctl-postgres --timeout=120s
   ```

   ```yaml
   apiVersion: batch/v1
   kind: Job
   metadata:
     name: codexctl-migrate
   spec:
     ttlSecondsAfterFinished: 86400
     template:
       metadata:
         labels:
           app.kubernetes.io/name: codexctl-migration
       spec:
         securityContext:
           runAsUser: 10001
           runAsGroup: 10001
           runAsNonRoot: true
           fsGroup: 10001
           fsGroupChangePolicy: OnRootMismatch
           seccompProfile: { type: RuntimeDefault }
         restartPolicy: Never
         initContainers:
           - name: prepare-secrets
             image: busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
             command: [sh, -ec]
             securityContext:
               allowPrivilegeEscalation: false
               readOnlyRootFilesystem: true
               capabilities: { drop: [ALL] }
             args:
               - >-
                 cp /projected/vault-key /keys/vault-key;
                 cp /projected/oidc-client-secret /keys/oidc-client-secret;
                 cp /projected/metrics-token /keys/metrics-token;
                 chmod 600 /keys/vault-key /keys/oidc-client-secret /keys/metrics-token
             volumeMounts:
               - { name: projected, mountPath: /projected, readOnly: true }
               - { name: keys, mountPath: /keys }
         containers:
           - name: migrate
             image: 767398060436.dkr.ecr.us-east-1.amazonaws.com/codexctl-central@sha256:1b7a83ab6fed64a146f0eeb391d40bdd4bd3829f8466a83cf6c10aa4fbf0c515
             command:
               [
                 codexctl-central,
                 migrate,
                 --state,
                 /data/state,
                 --key-file,
                 /keys/vault-key,
               ]
             securityContext:
               allowPrivilegeEscalation: false
               readOnlyRootFilesystem: true
               capabilities: { drop: [ALL] }
             env:
               - { name: CODEXCTL_CENTRAL_STORE, value: postgres }
               - name: CODEXCTL_CENTRAL_DB_CA_FILE
                 value: /etc/codexctl/rds-ca/global-bundle.pem
               - {
                   name: DB_HOST,
                   valueFrom:
                     {
                       secretKeyRef:
                         { name: codexctl-postgres, key: db-hostname },
                     },
                 }
               - {
                   name: DB_PORT,
                   valueFrom:
                     {
                       secretKeyRef: { name: codexctl-postgres, key: db-port },
                     },
                 }
               - {
                   name: DB_NAME,
                   valueFrom:
                     {
                       secretKeyRef: { name: codexctl-postgres, key: db-name },
                     },
                 }
               - {
                   name: DB_USER,
                   valueFrom:
                     {
                       secretKeyRef: { name: codexctl-postgres, key: db-user },
                     },
                 }
               - {
                   name: DB_PASSWORD,
                   valueFrom:
                     {
                       secretKeyRef:
                         { name: codexctl-postgres, key: db-password },
                     },
                 }
             volumeMounts:
               - {
                   name: rds-ca,
                   mountPath: /etc/codexctl/rds-ca,
                   readOnly: true,
                 }
               - { name: state, mountPath: /data }
               - { name: keys, mountPath: /keys, readOnly: true }
         volumes:
           - name: rds-ca
             configMap: { name: codexctl-rds-ca }
           - name: state
             persistentVolumeClaim: { claimName: state-codexctl-0 }
           - name: projected
             secret: { secretName: codexctl-secrets, defaultMode: 288 }
           - name: keys
             emptyDir: { medium: Memory }
   ```

   Keep `fsGroupChangePolicy: OnRootMismatch`, as the StatefulSet does. With the
   default policy, kubelet adds group read and write to every file on the claim
   at mount. `vault::private_read` then refuses those files, `backfill` reports
   pending logins, and `codexctl-0` cannot read its own state after a rollback
   (B33 attempt 3, 2026-10-09).

   Apply the reviewed Job, wait for completion, and repeat the manifest with
   `name: codexctl-backfill-initial` and `command: [codexctl-central, backfill, --state, /data/state, --key-file, /keys/vault-key]`.
   Keep both Job logs and the backfill JSON counts as the migration receipt.

3. **Re-backfill after quiescing.** PostgreSQL mode is the only serving mode;
   `dual` is migration-only and the server refuses to start in it. Run the
   final backfill from a reviewed one-shot migration Job that mounts the
   retained PVC. Keep the StatefulSet scaled to zero after this point. Do not
   run two refresh writers:

   ```sh
   kubectl --context plat-staging -n codexctl apply -f - <<'YAML'
   apiVersion: batch/v1
   kind: Job
   metadata:
     name: codexctl-backfill
   spec:
     ttlSecondsAfterFinished: 86400
     template:
       metadata:
         labels:
           app.kubernetes.io/name: codexctl-migration
       spec:
         securityContext:
           runAsUser: 10001
           runAsGroup: 10001
           runAsNonRoot: true
           fsGroup: 10001
           fsGroupChangePolicy: OnRootMismatch
           seccompProfile: { type: RuntimeDefault }
         restartPolicy: Never
         initContainers:
           - name: prepare-secrets
             image: busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
             command: [sh, -ec]
             securityContext:
               allowPrivilegeEscalation: false
               readOnlyRootFilesystem: true
               capabilities: { drop: [ALL] }
             args:
               - >-
                 cp /projected/vault-key /keys/vault-key;
                 cp /projected/oidc-client-secret /keys/oidc-client-secret;
                 cp /projected/metrics-token /keys/metrics-token;
                 chmod 600 /keys/vault-key /keys/oidc-client-secret /keys/metrics-token
             volumeMounts:
               - {name: projected, mountPath: /projected, readOnly: true}
               - {name: keys, mountPath: /keys}
         containers:
           - name: backfill
             image: 767398060436.dkr.ecr.us-east-1.amazonaws.com/codexctl-central@sha256:1b7a83ab6fed64a146f0eeb391d40bdd4bd3829f8466a83cf6c10aa4fbf0c515
             command: [codexctl-central, backfill, --state, /data/state, --key-file, /keys/vault-key]
             securityContext:
               allowPrivilegeEscalation: false
               readOnlyRootFilesystem: true
               capabilities: { drop: [ALL] }
             env:
               - {name: CODEXCTL_CENTRAL_STORE, value: postgres}
               - {name: CODEXCTL_CENTRAL_DB_CA_FILE, value: /etc/codexctl/rds-ca/global-bundle.pem}
               - {name: DB_HOST, valueFrom: {secretKeyRef: {name: codexctl-postgres, key: db-hostname}}}
               - {name: DB_PORT, valueFrom: {secretKeyRef: {name: codexctl-postgres, key: db-port}}}
               - {name: DB_NAME, valueFrom: {secretKeyRef: {name: codexctl-postgres, key: db-name}}}
               - {name: DB_USER, valueFrom: {secretKeyRef: {name: codexctl-postgres, key: db-user}}}
               - {name: DB_PASSWORD, valueFrom: {secretKeyRef: {name: codexctl-postgres, key: db-password}}}
             volumeMounts:
               - {name: rds-ca, mountPath: /etc/codexctl/rds-ca, readOnly: true}
               - {name: state, mountPath: /data}
               - {name: keys, mountPath: /keys, readOnly: true}
         volumes:
           - name: rds-ca
             configMap: { name: codexctl-rds-ca }
           - name: state
             persistentVolumeClaim: {claimName: state-codexctl-0}
           - name: projected
             secret: {secretName: codexctl-secrets, defaultMode: 288}
           - name: keys
             emptyDir: {medium: Memory}
   YAML
   kubectl --context plat-staging -n codexctl wait --for=condition=complete job/codexctl-backfill --timeout=10m
   kubectl --context plat-staging -n codexctl logs job/codexctl-backfill
   ```

4. **Cut over to the build-only HA overlay.** After the final backfill,
   build and apply the separately reviewed overlay (the overlay is deliberately
   not referenced by `deploy/k8s/staging-application.yaml`):

   ```sh
   kustomize build deploy/k8s/overlays/staging-ha
   kubectl --context plat-staging apply -k deploy/k8s/overlays/staging-ha
   kubectl --context plat-staging -n codexctl rollout status deployment/codexctl-ha
   ```

   The HA service accepts token, registry, add, and login renewal operations.
   Login status and cancellation use shared PostgreSQL journals and identity
   reservations. Enrollment and reset redemption remain unavailable. PR3 adds
   Linux login recovery, but live crash/takeover acceptance and rollout approval
   remain required before the B33 cutover.

   Verify three ready pods on separate hostnames/zones, no broker PVC mounts,
   `/ready` database health, one upstream refresh for a simultaneous forced
   request, and no lease or ExternalSecret errors before changing traffic.
   Change the staging `Ingress/codexctl` backend from `Service/codexctl` to
   `Service/codexctl-ha` and apply the Ingress change only after validation.

5. **Rollback.** Stop HA refreshes, mark the HA Service endpoints unready, and
   fence their leases by allowing the TTL to expire or explicitly releasing
   them. Restore the previous single-pod StatefulSet from the retained PVC
   snapshot, unset PostgreSQL mode, and verify file-mode `/ready`. Because HA
   may have rotated credentials after the snapshot, require an explicit
   relogin for every affected account and verify a successful file-mode token
   request before routing traffic back. Change the staging `Ingress/codexctl`
   backend from `Service/codexctl-ha` back to `Service/codexctl` and apply the
   Ingress change. Never run old and HA refresh writers
   simultaneously; preserve the database and PVC receipts for reconciliation.

## 6. Test and acceptance plan

Run these tests against two or three real pods and a disposable PostgreSQL
instance, then repeat the destructive cases in staging.

1. **Concurrent refresh.** Seed one account with a refresh token. Send forced
   token requests simultaneously to two pods, record upstream calls, and assert
   exactly one `refreshToken=true` call, one new refresh-token revision, and two
   successful responses. Repeat while the follower starts with an older access
   token.
2. **Leader kill during a request.** Kill the lease holder after the upstream
   response but before the commit, and again after the commit but before the HTTP
   response. Assert the successor either commits the single rotation or reads
   the committed revision; a stale holder cannot write after epoch change.
3. **Unresolved provider call.** Force lease renewal loss while the old
   provider call is still unresolved. Keep the old owner fenced and assert that
   no successor calls the provider until the old child is settled or confirmed
   stopped and its rotated credential is reconciled.
4. **Node drain.** Drain the node hosting the refresh holder. Confirm the PDB
   keeps one ready pod, topology rules place replacements on another node and
   zone, and token requests recover within 60 seconds. Check that no RWO
   Multi-Attach event is possible because broker pods have no shared RWO claim.
5. **Enrollment and renewal.** Assert that enrollment returns `503`. For renewal, assert cross-pod
   status/cancel, completed-request retries, continued token service during
   device polling, and one refresh owner during verification. On Linux, kill the
   device-login replica and retry with a fresh ID on another replica. Also check
   candidate takeover and unresolved verification without another refresh.
   Keep these live acceptance results separate from PR3's local tests.
6. **Reset idempotency.** In phase three, assert that reset redemption returns
   `503` on every pod. Move the one-spend and terminal-receipt assertions to
   phase four, after the shared journal is delivered
   (`src/central/resets.rs:325-441`).
7. **Readiness and fencing.** Remove the lease or database access from one pod;
   `/ready` must fail for unsafe write service, reads must either use committed
   state or return a bounded unavailable response, and metrics must identify the
   pod and lease epoch without secrets.
8. **Load and recovery.** Run account listing at the 60-second cache interval,
   mixed token requests, and a rolling restart. Verify no refresh race, no lost
   pending operation, and no account identity or revision regression. Record
   p50/p95/p99 token latency and the maximum observed outage; acceptance is
   p99 recovery and token success within 60 seconds after one pod/node loss.
