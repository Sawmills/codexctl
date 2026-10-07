# Runbook: B33 Attempt 3, Staging Cutover from File Mode to PostgreSQL HA

**Decision.** Run attempt 3 as four GitOps PRs to `deploy/k8s/overlays/staging` plus two one-shot migration Jobs, driven by one Claude Code operator that never uses a codexctl token. Do not start before SAW-12484 PR1, PR2, and PR3 are merged and shipped in one image. Rollback is a revert of the latest cutover PR, with one exception: after the first HA pod is Ready, a return to file mode also needs a relogin of every account whose refresh token rotated (see [Rollback](#rollback)).

Reader: the cutover operator and the HQ that approves the window. The design and the Job manifests are in [central-ha.md section 5](../../central-ha.md#5-staging-migration-and-rollback); this runbook changes how they are delivered, not what they do.

## Why Attempt 2 Stopped

Attempt 2 merged the writer stop (#130), and Argo scaled `codexctl-0` to zero. The lane driving the cutover ran Codex through codexctl tokens, so it lost its own model access when the server stopped. Nobody could run the backfill, the activation, or the proof, and Amir reverted with #135 (handoff-b33).

## Prerequisites (All Required)

1. **SAW-12484 merged and deployed in one image.** PR1 renewal (#143, merged `09ad744`), PR2 add-account in PostgreSQL mode, PR3 crash and takeover recovery. Section 4 of `central-ha.md` still says the intermediate release "does not authorize the B33 cutover"; PR3 must remove that line, or HQ records why it stands.
2. **Image pin.** One `Central server image` run on a main commit that contains PR3. Its digest goes into the HA Deployment and both Jobs. The staging-ha overlay pins `sha256:ecf8c1db…` and the Jobs in `central-ha.md` pin `sha256:2dfcb874…`; both are stale.
3. **Database.** `ExternalSecret/codexctl-postgres` is `Ready=True` (true on 2026-10-07) and the database holds no rows from an earlier attempt, or the operator records the counts before the backfill.
4. **Operator.** One Claude Code session named by HQ, not a Codex lane. Its credentials: AWS SSO (`plat-staging/AdministratorAccess` through `kubie`) for Kubernetes, the repository `gh` identity for merges, nothing from codexctl. Its claudectl account must not depend on the window. HQ checks this before the notice.
5. **Amir reachable** for the whole window. Only he can approve device codes, and a late rollback needs them.
6. **No other staging change in the window:** no image pin, no SSO cutover (SAW-12467), no loan or rename work.

## The Four PRs

Each PR changes only `deploy/k8s/overlays/staging` and is reviewed before the window. The Application `codexctl` auto-syncs `main` with prune and selfHeal, so a merge is a deploy. A manual `kubectl apply -k deploy/k8s/overlays/staging-ha` is forbidden: the two overlays share `Ingress/codexctl`, `ConfigMap/codexctl-sso`, `ExternalSecret/codexctl-secrets`, and `NetworkPolicy/codexctl`, and selfHeal would revert a manual change.

| PR | Change | Gate after Argo syncs |
|----|--------|-----------------------|
| W (writer stop) | `replicas: codexctl count: 0`, as #130 | `codexctl-0` deleted; PVC `state-codexctl-0` Bound |
| H (HA pods) | Add `Deployment/codexctl-ha`, `Service/codexctl-ha`, `PodDisruptionBudget/codexctl-ha` from staging-ha with the prerequisite digest. Keep the StatefulSet at 0 and keep its PVC. Retarget `monitoring.yaml` to the HA pods (per pod, since one Service target reaches one random replica) | Three pods Ready on separate hosts and zones; `/ready` database health; no lease or ExternalSecret errors |
| I (Ingress) | `Ingress/codexctl` backend `Service/codexctl` → `Service/codexctl-ha` | `/ready` 200 through the public host |
| C (cleanup, later) | Remove the StatefulSet and the file-mode Service after one stable week | None in the window |

PR H must not remove `PrometheusRule/codexctl` or `ScrapeConfig/codexctl`: the staging-ha overlay lacks both, and prune would delete them. To cut the Argo poll delay (about five minutes on 2026-10-07), the operator may request a refresh with `kubectl -n argocd annotate application codexctl argocd.argoproj.io/refresh=normal --overwrite`.

## Window

1. **Notice.** Send the fleet notice through the guide `w9C:tC` at least 15 minutes before PR W. It states the start time, the token outage from PR W until PR I passes (plan for 45 minutes), and that HQs park their Codex lanes. A lane left running stalls: its `central-token` call times out after 210 seconds (`timeout_ms` in the provider config).
2. **Backup.** With `codexctl-0` still serving, take the PVC archive and its SHA-256 as in `central-ha.md` step 1. Write the hash to the questions file.
3. **Merge PR W.** Wait for the gate.
4. **Migrate.** Apply the `codexctl-migrate` Job with the prerequisite digest and wait for completion. If `migrate` reports legacy lease rows, run it again with `--confirm-legacy-owners-settled` only after the operator confirms that no HA pod has ever run (`deploy/codexctl-ha` absent on 2026-10-07). Keep the log.
5. **Backfill.** Apply `codexctl-backfill` and keep its JSON counts. Compare users, accounts, and devices with the file state from the backup. Stop on any mismatch; the rollback is still a clean revert of PR W.
6. **Merge PR H.** This is the point of no clean return: a Ready HA pod starts refresh owners under the database lease, and a refresh can rotate that account's refresh token in PostgreSQL only.
7. **Validate before traffic.** Through `kubectl port-forward svc/codexctl-ha`: `/ready` 200, one forced simultaneous token request per account yields one upstream refresh, and the logs show no `owner_unavailable`.
8. **Merge PR I.** Then prove from a Mac: `/ready` 200 on `https://codexctl.ue1.staging.plat.sm-svc.com`, one `codexctl codex --account amir@sawmills.ai+personal exec "say ok"`, one renewal status call, and `codexctl list` on a second machine.
9. **Release the fleet** through the guide with the result and the time.

## Rollback

- **Before PR H (steps 3 to 5):** revert PR W. `codexctl-0` returns on the same PVC and the file state is still authoritative. This is the attempt-2 rollback.
- **After PR H, before any HA refresh:** revert PR I if merged, then PR H, then PR W. The operator checks the HA logs for refreshes first; with none, the file state is still valid.
- **After an HA refresh:** revert the same PRs, but the file state now holds a spent refresh token for every account the HA pods refreshed. Each such account needs `codexctl login <alias>` and Amir's device code before it serves again. Prefer a forward fix in HA when the fault allows it.
- Never run the StatefulSet and the HA Deployment as refresh writers at the same time. Keep the PVC archive, both Job logs, and the database until HQ closes B33.

## Done

Staging serves through `Service/codexctl-ha` with three Ready pods; tokens, renewal, and add-account work through the public host; monitoring and alerts still fire; HQ records the backup hash, Job receipts, and proofs on the B33 ticket.
