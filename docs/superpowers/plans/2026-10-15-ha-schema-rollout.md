# HA schema before serve implementation plan

**Goal:** Every PostgreSQL HA pod applies its image's schema successfully before starting the broker, including additive migrations that retain schema version 8.

**Architecture:** Reuse the existing deep migration module through `codexctl-central migrate`; Kubernetes init completion is the startup interface. Keep schema serialization and compatibility checks inside that module, and give the initializer the same database connection configuration as the broker. Do not add a second migration implementation or implicit migration inside `serve`.

**Tech stack:** Rust CLI, PostgreSQL, Kubernetes init containers, Kustomize, synthetic OIDC integration tests.

**Status:** HQ rule 57 APPROVED 2026-10-15T23:0xZ with the source-first amendment below. Conditions: failed init leaves two old pods serving; three racing inits on a current layout-8 schema finish well inside the 2-second DB budget; update both overlays, using existing DB inputs and CA only. Rollout still needs separate HQ GO and t56 watch. PR B base: `65c0ac4bdff496625029eed621faac9c36505b19`.

**Approved amendment and order:** PR A applies a transaction-local 100ms DDL lock timeout after admission, preserves the outer2s budget, logs the fixed lock-timeout class, and proves recovery after one retry per each of three racing migration processes under synthetic dashboard traffic. PR A #210 landed as `65c0ac4bdff496625029eed621faac9c36505b19` with exact-head Architect/HQ approval and all checks green. Its image must build and be verified before PR B pins it and enables init migration in both overlays. PR B changes no production Rust or historical Job/writer image pins. Rollout still needs separate HQ GO and t56 watch; the applied-schema fingerprint remains a deferred SAW-12757 item.

**Verified image:** run `38827955852` succeeded on landed PR A SHA; its `central-image` artifact and manifest publisher agree on `sha256:b1b1cc38ab45e882db4a25f71a171f203938da3e329372a2d1ac90b10fa2822f`. PR B pins only HA Deployments to this multi-architecture image; historical Job/writer pins remain unchanged.

## Evidence and scope

- HQ observed SQLSTATE 42703 at `browser_session_create` on 2026-10-15 22:24:04Z. The a5 migration image predates the additions to `browser_sessions` and `dashboard_audit`. HQ's manual migration exited 0 at 22:35Z.
- Both HA Deployment manifests currently initialize local state only, with no PostgreSQL environment on `initialize`. `serve` does not migrate (`src/bin/codexctl-central.rs`).
- `PostgresStore::migrate` (`src/central/storage.rs`) takes the schema-scoped transaction advisory admission lock before transactional DDL. `PostgresStore::connect` rejects a newer schema. `migrate_identity` returns immediately when the identity migration is already complete (`src/central/storage/migration.rs`). Re-running migration must preserve that completed fence.
- The #187 changes are additive and deliberately retain version 8. A version-number comparison alone cannot detect the missing column/table. Migration must run even when local `users.json` exists and version 8 is already recorded.
- Earlier #207 fixed a separately reproduced admission-contention defect; this plan addresses the now-confirmed live missing-schema cause.

## Task 1: reproduce the rollout gap at the startup interface

Files:

- Modify `tests/central_ha_startup_test/dashboard.rs`.
- Add test-only startup helpers under `tests/central_ha_startup_test/` only if they materially simplify ownership of the initialization operation.

1. Reuse the existing isolated PostgreSQL schema, synthetic issuer and real CLI fixture. Split fixture preparation from broker launch just enough to run the initializer before any broker starts; keep the default path for existing tests unchanged.
2. Build the actual pre-#187 shape: completed identity migration/version 8, `browser_sessions` without `auth_time_verified`, no `dashboard_audit`. Preserve a synthetic legacy session/user/device so the test also checks retention and absence of new fresh-sign-in authority.
3. Execute each checked-in HA `initialize` command through `sh -ec` with the real test CLI, replacing only the fixed container state/key paths with temporary paths. Read its actual JSON-style `args` array with a strict, test-only extractor; reject an unexpected manifest shape rather than silently substituting a copied command. Assert the expected shell command too. Local rendered-manifest verification in task 3 checks that Kustomize produces those same commands and environment references.
4. Start the broker without any helper-side `migrate` after downgrading the schema. Complete the synthetic OIDC callback and require success. Record RED from the current initializer: sign-in fails with 503 and bounded diagnostics contain SQLSTATE 42703 at `browser_session_create`. The existing fixture's unconditional migration must not mask this failure.

Run after approval, with a fresh loopback-only synthetic PostgreSQL and its URL injected into the test environment:

```sh
~/Code/.p122/testslot.sh env CARGO_BUILD_JOBS=2 cargo test -j 2 --locked --features central-real-db-tests --test central_ha_startup_test postgres_ha_rollout -- --nocapture --test-threads=1
```

## Task 2: minimal initializer fix and end-to-end GREEN

Modify only the HA initializer in:

- `deploy/k8s/overlays/staging/ha-deployment.yaml`
- `deploy/k8s/overlays/staging-ha/deployment.yaml`

Keep setup conditional, then run migration unconditionally in the existing `sh -ec` command:

```sh
if [ ! -f /data/state/users.json ]; then
  codexctl-central setup --state /data/state --key-file /keys/vault-key
fi
codexctl-central migrate --state /data/state --key-file /keys/vault-key
```

- Copy the broker's PostgreSQL store mode, five DB Secret key references and CA-file configuration to `initialize`. Add the broker's existing read-only `rds-ca` ConfigMap volume mount to init too; the RDS bundle is mounted, not baked into the image. Keep TLS enabled and retain the existing private read-only key mount.
- The image used by initialization must equal the broker's image in the rendered deployment, so future pin changes run the matching schema code.
- Do not swallow migration failures, background migration, add a settlement override, weaken the 2-second DB budget, or replay any historical backfill Job. Kubernetes may retry a failed init; the new broker must never start on its failure.
- Re-run task 1 and retain GREEN.
- After that same upgrade, sign in freshly, create a synthetic target machine and API caller, submit the dashboard's real same-origin Revoke form on another pod, and assert persisted `revoked=true` plus the expected actor/operation/target/result in `dashboard_audit`. Verify duplicate dashboard response 409 and repeated API revocations 204. Assert no provider refresh/token rotation.
- Verify legacy sessions/rows survive, with legacy `auth_time_verified=false`.

PR B changes no production Rust; the approved timeout policy is already supplied by PR A's verified image. If tests expose another migration defect, stop and amend this plan before extending scope.

## Task 3: concurrency and failure behavior

- Run three actual initializer processes against one synthetic pre-#187 database and then repeat initialization with local state already present. Require an intact completed identity fence, preserved rows and successful subsequent sign-in; normal concurrent initialization must serialize successfully without duplicated schema or audit state.
- Keep one existing broker serving during a repeated migration and verify existing session access afterward. This is evidence for the present additive upgrade, not permission for future destructive rolling migrations.
- Hold the admission lock past the DB budget: initialization must exit nonzero, the transaction must leave no partial additions, and a broker launch marker chained behind init must remain absent. Release it, then verify a fresh initialization succeeds.
- Repeat the no-serve assertion for a synthetic unsupported future schema. Preserve existing compatibility rejection.
- Extend local rendered checks (the established `tests/ha_lifecycle_test.py` or one focused adjacent suite) to compare both overlays' initializer DB/CA configuration, same-image requirement and startup ordering. Do not add workflow wiring; A124 remains in force.

```sh
/usr/bin/python3 tests/ha_lifecycle_test.py -v
kubectl kustomize deploy/k8s/overlays/staging
kubectl kustomize deploy/k8s/overlays/staging-ha
```

Only local render output is inspected; no cluster connection or Secret read.

## Task 4: document, review and handoff

- Update `docs/central-ha.md` to distinguish the completed one-time cutover from ongoing HA upgrades: setup, serialized migration, then serve; migration failure stalls a pod's init. Additive rolling changes only; changes requiring drained writers keep their separate reviewed maintenance procedure.
- Preserve replicas, maxUnavailable=1/maxSurge=0, PDB, lifecycle, storage custody, historical Jobs and writer image pins. Local renders must show only the intended HA initializer and HA image changes.
- Run focused startup/dashboard regressions, formatting, locked all-target/all-feature Clippy, then required CI. Heavy Rust commands always use `~/Code/.p122/testslot.sh`, `CARGO_BUILD_JOBS=2`, cargo `-j 2`; require at least 40 GiB free before building and clean only this worktree's target after green.
- Apply ask-matt at the code/review/PR transitions; use TDD at the startup interface, independent Standards/Spec review and final autoreview. One App-authored PR linked to SAW-12757, then exact-head Architect and HQ rule 56 review plus required green checks.
- Deployment needs HQ roll GO and t56 watch. Keep at least two Ready pods, finish at three Ready across zones, verify init exit 0 per new pod, then HQ/Amir sign-in and synthetic-machine Revoke acceptance. No credential values in receipts. Rollback is a reviewed manifest revert, leaving additive DB objects intact; never drop schema objects on rollback.
- This lane performs no cluster action during planning or local verification.
