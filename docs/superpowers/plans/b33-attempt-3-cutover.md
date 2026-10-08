# Runbook: B33 Attempt 3, Staging Cutover from File Mode to PostgreSQL HA

**Decision.** Run attempt 3 as four GitOps PRs to `deploy/k8s/overlays/staging` plus two one-shot migration Jobs, driven by one Claude Code operator that never uses a codexctl token. Do not start before SAW-12484 PR1, PR2, and PR3 are merged and shipped in one image. Rollback is a revert of the latest cutover PR, with one exception: after the first HA pod is Ready, a return to file mode also needs a relogin of every account whose refresh token rotated (see [Rollback](#rollback)).

Reader: the cutover operator and the HQ that approves the window. The design and the Job manifests are in [central-ha.md section 5](../../central-ha.md#5-staging-migration-and-rollback); this runbook changes how they are delivered, not what they do.

## Readiness Check (2026-10-08)

Checked against `main` at `c7fd94e` after SAW-12484 PR1 (#143), PR2 (#145), and PR3 (#154) merged. Verdict: **not ready to schedule** until the prerequisite 7 fix ships. HQ decided both open questions on 2026-10-08: the server waits for a held lease (prerequisite 7, option a), and window step 7 runs the named acceptance items (prerequisite 1).

| Item                         | State                                                                                                                                                   | Action                                                                 |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| PR1, PR2, PR3 merged         | #143 `09ad744`, #145 `f89d3d8`, #154 `c7fd94e`                                                                                                          | None                                                                   |
| Image with PR3               | `Central server image` run on `c7fd94e` passed; index `sha256:d2d5b558f16edb4a53de95608ecb4c7dc7fe230d10ce37a1e36e5a6716de2c72` (tag `sha-c7fd94e80a4`) | Confirm the index digest in ECR, then pin it in PR H and both Jobs     |
| `central-ha.md` cutover line | Section 5 still says live crash/takeover acceptance and rollout approval "remain required before the B33 cutover"                                       | Decided: window step 7 runs section 6 items 1, 4, 5, 7                 |
| Schema                       | `migrate` brings an empty database to layout version 7: PR1 and PR2 login journals, PR3 `central_login_holders`                                         | None; the emptiness query already covers new tables                    |
| Cross-replica token requests | **Blocker.** See prerequisite 7                                                                                                                         | Decided: server lease wait (~15 s) in a follow-up PR before the notice |
| Pending file-mode work       | `backfill` refuses pending logins but not interrupted renames                                                                                           | Window step 2 pre-check                                                |
| Rollback after PR H          | Accounts added through HA and in-flight login journals exist only in PostgreSQL                                                                         | Rollback steps updated                                                 |

## Why Attempt 2 Stopped

Attempt 2 merged the writer stop (#130), and Argo scaled `codexctl-0` to zero. The lane driving the cutover ran Codex through codexctl tokens, so it lost its own model access when the server stopped. Nobody could run the backfill, the activation, or the proof, and Amir reverted with #135 (handoff-b33).

## Prerequisites (All Required)

1. **SAW-12484 merged and deployed in one image.** PR1 renewal (#143, `09ad744`), PR2 add-account in PostgreSQL mode (#145, `f89d3d8`), PR3 crash and takeover recovery (#154, `c7fd94e`). PR3 did not remove the gate line: section 5 of `central-ha.md` still says "live crash/takeover acceptance and rollout approval remain required before the B33 cutover". HQ decision (2026-10-08): window step 7 runs `central-ha.md` section 6 items 1, 4, 5, and 7.
2. **Image pin.** One `Central server image` run on a main commit that contains PR3. Its digest goes into the HA Deployment and both Jobs. The staging-ha overlay pins `sha256:ecf8c1db…` and the Jobs in `central-ha.md` pin `sha256:2dfcb874…`; both are stale. The run on `c7fd94e` pushed index `sha256:d2d5b558…` to tag `sha-c7fd94e80a4`; the operator confirms it with `aws ecr describe-images --repository-name codexctl-central --image-ids imageTag=sha-c7fd94e80a4` before pinning.
3. **Database.** `ExternalSecret/codexctl-postgres` is `Ready=True` (true on 2026-10-07), and the target database is empty. A count receipt is not enough: `backfill` skips an account whose stored revision is at least the file revision and ignores user and device conflicts (`src/central/storage.rs`), so stale database rows survive it. The operator proves emptiness with the read-only [count query](#read-only-queries) before step 4. If any codexctl table holds a row, stop; resetting the database is a separate HQ decision with the infra owner.
4. **Operator.** One Claude Code session named by HQ, not a Codex lane. Its credentials: AWS SSO (`plat-staging/AdministratorAccess` through `kubie`) for Kubernetes, the repository `gh` identity for merges, nothing from codexctl. Its claudectl account must not depend on the window. HQ checks this before the notice.
5. **Amir reachable** for the whole window. Only he can approve device codes, and a late rollback needs them.
6. **No other staging change in the window:** no image pin, no SSO cutover (SAW-12467), no loan or rename work.
7. **Cross-replica token requests (blocker, HQ decision).** In PostgreSQL mode every token request takes the account's database lease, starts a native owner child, and stops it before the lease is released (`src/central/managed.rs`, token path). A second request for the same account on another pod gets `503 refresh_in_progress` at once (`acquire_lease` makes one attempt), and the client does not retry it (`src/central/native.rs`, token request). The HA test `three_postgres_servers_start_without_refresh_children_and_launch_only_under_lease` asserts that 503. Acceptance item 1 in `central-ha.md` section 6 expects "two successful responses". Behind one Service with three pods, fleet lanes that share an account will see token failures that file mode never shows, because file mode waits on the in-process owner lock. Options: (a) the server retries the lease for a bounded time (for example 15 s) before it returns 503; (b) the client retries `refresh_in_progress` with backoff; (c) accept and measure in step 7. HQ decision (2026-10-08): (a), a bounded server wait of about 15 s, delivered as a SAW-12484 follow-up PR with a two-pod concurrency test and shipped in the pinned image before the notice.

## The Four PRs

Each PR changes only `deploy/k8s/overlays/staging` and is reviewed before the window. The Application `codexctl` auto-syncs `main` with prune and selfHeal, so a merge is a deploy. A manual `kubectl apply -k deploy/k8s/overlays/staging-ha` is forbidden: the two overlays share `Ingress/codexctl`, `ConfigMap/codexctl-sso`, `ExternalSecret/codexctl-secrets`, and `NetworkPolicy/codexctl`, and selfHeal would revert a manual change.

| PR                 | Change                                                                                                                                                                                                                                                                                    | Gate after Argo syncs                                                                                     |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| W (writer stop)    | `replicas: codexctl count: 0`, as #130                                                                                                                                                                                                                                                    | `codexctl-0` deleted; PVC `state-codexctl-0` Bound                                                        |
| H (HA pods)        | Add `Deployment/codexctl-ha`, `Service/codexctl-ha`, `PodDisruptionBudget/codexctl-ha` from staging-ha with the prerequisite digest. Keep the StatefulSet at 0 and keep its PVC. Retarget `monitoring.yaml` to the HA pods (per pod, since one Service target reaches one random replica) | Three pods Ready on separate hosts and zones; `/ready` database health; no lease or ExternalSecret errors |
| I (Ingress)        | `Ingress/codexctl` backend `Service/codexctl` → `Service/codexctl-ha`                                                                                                                                                                                                                     | `/ready` 200 through the public host                                                                      |
| C (cleanup, later) | Remove the StatefulSet and the file-mode Service after one stable week                                                                                                                                                                                                                    | None in the window                                                                                        |

PR H must not remove `PrometheusRule/codexctl` or `ScrapeConfig/codexctl`: the staging-ha overlay lacks both, and prune would delete them. To cut the Argo poll delay (about five minutes on 2026-10-07), the operator may request a refresh with `kubectl -n argocd annotate application codexctl argocd.argoproj.io/refresh=normal --overwrite`.

## Window

1. **Notice.** Send the fleet notice through the guide `w9C:tC` at least 15 minutes before PR W. It states the start time, the token outage from PR W until PR I passes (plan for 45 minutes), and that HQs park their Codex lanes. A lane left running stalls: its `central-token` call times out after 210 seconds (`timeout_ms` in the provider config).
2. **Backup.** With `codexctl-0` still serving, take the PVC archive and its SHA-256 as in `central-ha.md` step 1. Write the hash to the questions file. The notice also asks HQs to start no `relogin`, add, or rename in the window. After the backup, list pending work in the archive: any `account-logins/*` or `accounts/*` login record that is not terminal makes `backfill` refuse (step 5 then rolls back), and any `accounts/*/rename.json` is an interrupted rename that `backfill` does not check. If either exists, let `codexctl-0` finish it before PR W.
3. **Merge PR W.** Wait for the gate.
4. **Migrate.** Apply the `codexctl-migrate` Job with the prerequisite digest and wait for completion. Keep the log. An empty database has no legacy lease rows; if `migrate` reports any, stop the window. Attempt 3 never passes `--confirm-legacy-owners-settled`, because that flag needs settlement receipts for each earlier owner (`central-ha.md`, legacy lease handoff) that this window cannot produce.
5. **Backfill.** Apply `codexctl-backfill` and keep its JSON counts. Compare users, accounts, and devices with the file state from the backup. Stop on any mismatch; the rollback is still a clean revert of PR W.
6. **Merge PR H.** This is the point of no clean return: a Ready HA pod starts refresh owners under the database lease, and a refresh can rotate that account's refresh token in PostgreSQL only.
7. **Validate before traffic.** Through `kubectl port-forward` to each pod (a Service port-forward reaches one pod): `/ready` 200 on all three; one forced simultaneous token request per account, sent to two pods, yields one upstream refresh and two 200 responses; ten parallel token requests for one account across the three pods all return 200; and the logs show no `owner_unavailable` and no `refresh_in_progress` response. Record p50, p95, and p99 token latency. Run the `central-ha.md` section 6 items that HQ named for prerequisite 1.
8. **Merge PR I.** Then prove from a Mac: `/ready` 200 on `https://codexctl.ue1.staging.plat.sm-svc.com`, one `codexctl codex --account amir@sawmills.ai+personal exec "say ok"`, one renewal status call, and `codexctl list` on a second machine.
9. **Release the fleet** through the guide with the result and the time.

## Rollback

- **Before PR H (steps 3 to 5):** revert PR W. `codexctl-0` returns on the same PVC and the file state is still authoritative. This is the attempt-2 rollback.
- **After PR H:** fence the HA writers before the file writer returns, in this order:
  1. Revert PR I if merged. `Service/codexctl` has no endpoints, so token traffic stops.
  2. Revert PR H. Wait until no `codexctl-ha` pod exists (`kubectl -n codexctl get pods` shows none; the pods have a 60-second grace period).
  3. Run the read-only [lease and login queries](#read-only-queries). Every `account_refresh_leases` row must be released, including login leases (holder `<login holder>:<operation id>`). An unreleased row, even an expired one, means the owner's settlement is unknown: stop, keep PR W merged, and forward-fix in HA. A login operation that is not terminal may hold a grant that file mode cannot see: settle it in HA first, or record its account for relogin.
  4. Only then revert PR W.
- **Spent tokens:** if the HA logs show any refresh, the file state holds a spent refresh token for each refreshed account. Each such account needs `codexctl login <alias>` and Amir's device code before it serves in file mode. Prefer a forward fix in HA when the fault allows it.
- **HA-only credentials:** an account added or renewed through HA (PR2 add-account, PR1 renewal) has its credential only in PostgreSQL, and a `rejected` receipt can hold a captured candidate. List them with the login query. Before file mode serves, re-add each completed add and run `codexctl login <alias>` for each completed renewal and each account with `holds_candidate`.
- Never run the StatefulSet and the HA Deployment as refresh writers at the same time. Keep the PVC archive, both Job logs, and the database until HQ closes B33.

## Read-Only Queries

No `codexctl-central` command reports row counts or lease state, so the operator runs these as a one-shot Job with a `postgres:16` image, the `codexctl-migration` label (for the egress policy), the `codexctl-postgres` Secret as `PG*` environment variables, and `PGOPTIONS=-c default_transaction_read_only=on`. The Job is reviewed with PR W.

```sql
-- Emptiness (prerequisite 3): exact row count of every table that exists.
-- No output row, or 0 in every row, means empty.
SELECT table_name,
       (xpath('/row/c/text()',
              query_to_xml(format('SELECT count(*) AS c FROM %I.%I', table_schema, table_name),
                           false, true, '')))[1]::text::bigint AS row_count
FROM information_schema.tables
WHERE table_schema = current_schema() AND table_type = 'BASE TABLE'
ORDER BY table_name;

-- Lease fence (rollback step 3): must return 0.
SELECT count(*) FROM account_refresh_leases WHERE NOT released;

-- Login journals (rollback step 3 and HA-only credentials). Every row needs
-- action before file mode serves: a completed add or renewal and any receipt
-- with a captured candidate hold credentials only PostgreSQL has.
SELECT user_id, kind, phase, polling_clear, alias, landed_alias, account_id,
       candidate_workspace IS NOT NULL AS holds_candidate
FROM central_login_operations
WHERE phase NOT IN ('failed','canceled')
   OR candidate_workspace IS NOT NULL
ORDER BY user_id, alias;
```

The emptiness query names no table, so it also works before migration 1, when no table exists, and it counts tables added later, such as the loan tables. The lease and login queries run only after migration, when their tables exist.

## Done

Staging serves through `Service/codexctl-ha` with three Ready pods; tokens, renewal, and add-account work through the public host; monitoring and alerts still fire; HQ records the backup hash, Job receipts, and proofs on the B33 ticket.
