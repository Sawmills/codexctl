# Account server high availability design

**Status:** design only. This document does not change `src/` or `deploy/`.

The current staging shape is one StatefulSet replica with one encrypted
`ReadWriteOncePod` claim (`deploy/k8s/base/statefulset.yaml:9-13,134-142`). The
broker deliberately takes a process-local file lock before serving
(`src/central/managed.rs:1001-1004`), and the documented deployment says that a
second broker must not mount the state (`docs/central-server.md:386-390`). That
protects the current single-writer design but turns node eviction into a
storage-attach outage. The target is several pods on separate nodes and zones,
with durable state in PostgreSQL and an explicit refresh fence.

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
| SSO configuration, vault key, metrics credential | The SSO client secret is read from a private file at startup (`src/central/enrollment.rs:80-115`); the vault key and metrics token are projected secrets in staging (`deploy/k8s/base/statefulset.yaml:33-55`, `120-133`).                                                                                                                                                                              | Read-only after startup.                                                                                                                                                                                                                                               | Continue using External Secrets/secret mounts. Rotate the vault key only with a coordinated re-encryption migration, as already documented (`docs/central-server.md:392-400`).                                                                                                        |
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
  `deploy/k8s/base/statefulset.yaml:104-113` and `docs/central-server.md:467-472`.
- Use a short `preStop` drain: stop accepting new refresh acquisitions, finish
  in-flight persistence, release the account lease, and then terminate. A pod
  that loses the lease or database connection must fail readiness immediately.
- Remove the per-replica RWO claim from the broker. If a temporary compatibility
  release still mounts the old PVC, run exactly one writer and keep followers
  read-only; do not attach the RWO claim to multiple pods.

## 5. Staging migration and rollback

The migration is staged so no cutover step depends on a cold database import.
The operator measures the token-request outage at every step and aborts if it
approaches 60 seconds.

1. **Prepare and back up.** Freeze administrative mutations, verify the vault key
   and PVC snapshot, and export a checksummed copy of `users.json`,
   `devices.json`, every account vault/runtime journal, reset journals, and
   relogin records. Keep the existing pod serving token reads.
2. **Install the compatibility release.** Add the PostgreSQL schema, least-
   privilege role, connection secret, and migration job. Start the current
   single pod with dual-read/dual-write enabled: it remains the only refresh
   writer while it backfills rows under the existing owner lock. Reconcile row
   counts, account identities, token revisions, device hashes, pending reset
   IDs, and nonterminal relogin operations before proceeding.
3. **Warm followers.** Start two new pods in read-only/follower mode on distinct
   nodes and zones. They read the database, never attach the old PVC, and expose
   readiness only after catalog/device rows and the lease table are readable.
   Exercise `GET /v1/accounts`, `GET /v1/resets`, and an access-token request
   against each pod through an internal test Service.
4. **Enable fenced refresh.** Turn on per-account lease acquisition for the new
   image. Run a two-pod forced-refresh canary for one staging account and verify
   one upstream refresh, one committed revision, and identical responses from
   both pods. Keep the old pod as a read-only emergency endpoint during the
   canary.
5. **Cut over.** Mark the old pod unready, wait for its in-flight operations to
   drain, and release its leases. Point the Service at the three new pods. The
   old process must not be allowed to refresh after handoff. Observe token
   success, lease transitions, and database latency for at least two lease TTLs.
6. **Finish.** After a successful observation window, disable the file-backed
   dual-write path, retain the PVC snapshot for the agreed rollback period, and
   remove the old single-replica workload in a later reviewed deployment.

Rollback is a feature-flag reversal, not a simultaneous writer operation. If
the database or new pods fail, stop new-pod writes, fence their leases, mark the
new Service endpoints unready, and route to the still-running old pod. If the
old pod was stopped, restore its PVC snapshot and start exactly one old image;
the compatibility image reads the exported rows or file snapshot and refuses
to refresh until it has a current fencing epoch. This keeps one refresh writer
and bounds the outage to pod readiness plus one lease interval. Never force-
delete the old pod while its native owner may still be alive, matching the
existing recovery warning (`docs/central-server.md:454-456`).

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
3. **Node drain.** Drain the node hosting the refresh holder. Confirm the PDB
   keeps one ready pod, topology rules place replacements on another node and
   zone, and token requests recover within 60 seconds. Check that no RWO
   Multi-Attach event is possible because broker pods have no shared RWO claim.
4. **Enrollment and renewal.** Start enrollment on pod A, complete the browser
   callback on pod B, and poll from pod C. Repeat a login renewal across pods;
   assert one operation record, one candidate promotion, and no duplicate native
   owner.
5. **Reset idempotency.** Submit one redemption ID to two pods, kill one during
   the provider call, and retry from the other. Assert one credit/key spend and
   the same terminal receipt, as required by the current journal behavior
   (`src/central/resets.rs:325-441`).
6. **Readiness and fencing.** Remove the lease or database access from one pod;
   `/ready` must fail for unsafe write service, reads must either use committed
   state or return a bounded unavailable response, and metrics must identify the
   pod and lease epoch without secrets.
7. **Load and recovery.** Run account listing at the 60-second cache interval,
   mixed token requests, and a rolling restart. Verify no refresh race, no lost
   pending operation, and no account identity or revision regression. Record
   p50/p95/p99 token latency and the maximum observed outage; acceptance is
   p99 recovery and token success within 60 seconds after one pod/node loss.
