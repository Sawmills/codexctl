# SAW-12988 HA disruption and graceful shutdown implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Stop avoidable Karpenter consolidation handovers, eliminate the failing preStop lifecycle sequence, and roll the three staging HA replicas one at a time while retaining at least two Ready replicas.

**Architecture:** Treat the HA workload's lifecycle as one module, with the rendered Deployment/PDB and kubelet stop sequence as its interface. Keep graceful persistence and lease settlement in the existing account-server implementation; change the Kubernetes adapter instead of introducing a shutdown endpoint, new module, or runtime flag. Keep the tracked staging overlay and the render-only staging-ha overlay consistent.

**Tech Stack:** Kubernetes Deployment/PDB, Karpenter 1.8.6, Kustomize, Python/PyYAML, isolated Docker PID namespaces, existing Rust/PostgreSQL shutdown regression.

## Rule 57 decision and boundaries

HQ approved this plan at 2026-10-15T14:0xZ. At its 13:35Z submission, no source/manifests had been changed, no PR opened, and no cluster mutation or provider request had run. The local diagnostic containers have no network and use only synthetic process behavior.

HQ's deployment disposition is merge + Argo roll as soon as exact-head Architect, HQ and CI gates pass; t56 runs acceptance (at least two Ready during roll, final 3/3 Ready in three zones, public `/ready` 200, ten minutes without errors and a Mac turn). This disposition supersedes the proposed two-hour recurrence observation as a release gate. Longer observation remains useful but does not delay this approved rollout. HQ supplied the previously blocked metadata: default NodePool `expireAfter: 336h`, no `terminationGracePeriod`, consolidation after 1800 seconds, budget one node; live Argo e48dc23b Synced/Healthy and three named pods in three zones. HQ explicitly accepted the expiry/drain caveat documented below; no NodePool mutation is authorized.

Use one PR on `amiri/saw-12988-ha-disruption`, from main `e48dc23b332d2e98ecf449c27d8d3c8abbd1cc21`. Worktree: `/home/amir/Code/codexctl/.worktrees/SAW-12988`. Preserve the existing deferred Job items in SAW-12988; this PR covers only the newly assigned lifecycle/disruption work. The historical schema-inventory item was addressed during H/I rounds; do not reopen or modify the completed cutover Jobs here.

## Evidence and diagnosis

Read-only observation on 2026-10-15, approximately 13:25–13:35Z:

- Live broker image is still `codexctl-central@sha256:532fd08ad86c686ef46e71904703b709017c06f699b9ba218636354840a85ece` on all three pods.
- Events: `z9zn5` evicted `Underutilized` at 12:57:16Z, then `FailedPreStopHook` at 12:57:17Z. Its replacement `tqlhq` was evicted `Forceful Termination` at 13:01:32Z with `FailedPreStopHook` in the same second. HQ separately records d4bwl at 11:16Z and cvl7q at 12:1xZ. Do not infer that every forceful event was voluntary consolidation.
- Three current pods `bfg5t`, `d75p8`, `p85k6` are Ready, restart count 0, on three distinct nodes. Zone labels require a separate authorized read; HQ previously confirmed three zones.
- Live strategy is default RollingUpdate: `maxSurge: 25%`, `maxUnavailable: 25%`; at three replicas that permits one surge pod and zero unavailable replicas. Four pods can occupy three zones during a roll.
- Live PDB has `minAvailable: 1`, `currentHealthy: 3`, `desiredHealthy: 1`, `disruptionsAllowed: 2`. It allows two simultaneous evictions; changing only this PDB would still permit serial hourly consolidation.
- Both HA manifests run `sh -ec 'kill -TERM 1; sleep 5'` in preStop. The serving shell uses `exec codexctl-central`, so the account server is PID 1 (`overlays/staging/ha-deployment.yaml:120`, hook at :173–181; render-only counterpart :168–176). The runtime's source commit `97512c585f0b9b76ae523c43df84afe9fddcdfbb` and the plan base have identical `src/central/managed.rs` and `deploy/Dockerfile` (verified by git diff).
- `src/central/managed.rs:2831–2854` installs the TERM handler, stops admission/background recovery, waits for the HTTP server and drains work. Subsequent lines settle native children and snapshots; `tests/central_ha_startup_test.rs:541–611` exercises TERM during a rotation, transient persistence failure, and successor acquisition.
- Bounded sampled current-pod logs: 591 lines, zero `owner_unavailable`, `central owner shutdown`, or `server_stopping` markers. This is not an audit of deleted-pod shutdowns; their logs/exit codes are unavailable.

**Reproduced cause:** preStop sends TERM before finishing its own sleep. When PID 1 finishes shutdown quickly, the container runtime kills the remaining hook process in that PID namespace. The hook reports failure despite successful server exit. The local reproduction executes the actual checked-in hook with UID/GID 10001, all capabilities dropped, read-only filesystem and no network, against a synthetic TERM-handling PID 1 in the runtime's cached Node base image `b26b04c123d9`.

Diagnostic command already run:

```sh
python3 /home/amir/Code/.fleet/codexctl/receipts/saw-12988/prestop-repro.py
```

It fails the hook-success assertion: hook exit **137** in ~0.04 s, server exit **0**, settlement marker present, stderr empty. With a 250 ms settlement delay: hook exit **137**, server exit **0**. A candidate hook containing only `sleep 5`, followed by kubelet-style TERM after hook completion, returns hook **0** and server **0** for both settlement speeds. This reproduces the lifecycle defect; it does not prove the unavailable exit code of each historical pod.

Receipts: `/home/amir/Code/.fleet/codexctl/receipts/saw-12988/` (`prestop-repro.py`, `current-fast.txt`, `current-delayed.txt`, `candidate-fast.txt`, `candidate-delayed.txt`, `current-log-counts.json`). No live state or credentials enter these tests.

## Proposed change

Modify both Deployment manifests:

- `deploy/k8s/overlays/staging/ha-deployment.yaml`
- `deploy/k8s/overlays/staging-ha/deployment.yaml`

Add explicit integer rollout settings:

```yaml
spec:
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxSurge: 0
      maxUnavailable: 1
  template:
    metadata:
      annotations:
        karpenter.sh/do-not-disrupt: "true"
```

Replace preStop with a delay-only hook:

```yaml
lifecycle:
  preStop:
    exec:
      command: [sh, -ec, "sleep 5"]
```

The delay gives endpoint removal time to propagate. Kubelet sends TERM after the hook completes, invoking the existing graceful server path. The delay does not itself stop admission and is not a new lease protocol. Preserve `terminationGracePeriodSeconds: 60`; after the five-second hook, normal drain has approximately 55 seconds before the grace deadline. Existing docs already explain that prolonged settlement can exceed that budget and forced loss can require login renewal. This PR does not solve durable recovery from forced pod loss, and must not claim it does.

Modify both PDB manifests:

- `deploy/k8s/overlays/staging/ha-pdb.yaml`
- `deploy/k8s/overlays/staging-ha/pdb.yaml`

Replace `minAvailable: 1` with integer `maxUnavailable: 1` (never retain both fields). Keep each overlay's own selector unchanged. This protects eviction-based disruptions; the explicit Deployment strategy separately controls rolling updates. The two mechanisms do not create one global budget for concurrent unrelated failures.

Keep three replicas, existing topology/hostname constraints, runtime image and init image, probes, resource sizing, Services, database/Secret refs, network policy, file-writer replicas 0, original PVC and completed final-backfill Job unchanged. No workflow edits, image build or release.

## Verification, red first

### Task 1: Lock down lifecycle behavior at the rendered workload interface

Create `tests/ha_lifecycle_test.py`, using standard unittest + PyYAML and an explicitly pinned/cached synthetic image. It must never access a live account, state directory or network. Use `kubectl kustomize` locally; no cluster connection is needed for rendering.

1. Render both overlays and locate their Deployment and matching PDB by kind/name. Read each broker's actual rendered hook.
2. Add a regression that runs that hook inside the isolated synthetic PID namespace, checks successful hook completion while the process remains available until kubelet's subsequent TERM, then checks TERM settlement and exit 0. Cover quick settlement and a short in-flight settlement. The current hook must fail with the reproduced teardown result, not an unrelated environment error. Always remove only the test's own uniquely named container in `finally`.
3. Add rendered availability invariants: three replicas; integer `0/1` rolling update; annotation on the Pod template; PDB permits only one unavailable pod and selects the intended HA pods; required hostname anti-affinity and zone/hostname spread stay present.
4. Run `python3 tests/ha_lifecycle_test.py -v` and record the exact expected failures before modifying YAML. If Docker or the explicit cached image is absent, distinguish that setup failure from a red regression; never silently pass this lifecycle test.
5. Apply only the four YAML changes above; rerun to green.
6. Avoid a new production wrapper or generic shutdown library: the existing server owns persistence; Kubernetes owns termination ordering. This is the existing module's adapter change.

### Task 2: Check the existing graceful persistence contract

Use a disposable loopback PostgreSQL database and synthetic Codex fixture, through the shared test slot, never staging credentials. The existing regression is:

```sh
CARGO_BUILD_JOBS=2 ~/Code/.p122/testslot.sh cargo test -j 2 --locked --features central-real-db-tests --test central_ha_startup_test three_postgres_servers_start_without_refresh_children_and_launch_only_under_lease -- --exact
```

Supply the test's existing documented local `DATABASE_URL` setup. It must prove settlement survives a transient database error and the successor gets the persisted revision. Do not add a duplicate Rust test or change runtime source. No concurrent heavy Rust build. Run the repo's formatting/Clippy checks through the same slot if required for the PR; local Trunk is forbidden. Hosted CI remains authoritative; no lane workflow edits to add the Python command.

### Task 3: Validate both renders and document the policy

```sh
kubectl kustomize deploy/k8s/overlays/staging
kubectl kustomize deploy/k8s/overlays/staging-ha
```

Parse the before/after renders: only the intended Deployment lifecycle/annotation/strategy and PDB spec may differ. Check no Secret **payload** is emitted or read; these manifests contain only external references. Freeze all image fields. Update `docs/central-ha.md` section 4 and the lifecycle caveat near its beginning, plus `deploy/k8s/overlays/staging-ha/README.md`, so they describe the new policy and the build-only boundary. Keep historical cutover receipts/Job images untouched.

### Task 4: Review and publish one PR

Read ask-matt for the review and PR phases, run required autoreview, post one PR with the reproduction and rendered diff, then request exact-head Architect and HQ rule 56 review. All checks green and no unresolved review threads before the installed merge helper; App-token writes, no queue/admin alternate for this repo. Do not rerun failed CI jobs without a specific exception. Deployment is a separately coordinated action after HQ's accepted plan and guide notice.

## Deployment and acceptance proposal (t56 executes cluster work)

### Pre-deploy gates

1. HQ approves this plan and agrees a 30-minute rollout/initial-proof window; guide announces the potential handovers. Record start and hard end, with status every 15 minutes to questions.md; HQ relays to the guide. No live proof or cluster change in this planning phase.
2. t56 checks the actual serving NodePools/NodeClaims' `expireAfter` and `terminationGracePeriod`, pending node termination/disruption, three Ready pods in three zones, live Argo tracking/sync behavior, and the approved unchanged image. The devbox role is forbidden from reading NodePools/nodes and Argo Applications; do not change identity or retry by another route. Report these nonsecret fields via HQ. No nodepool/infra edits are proposed.
3. If a pool has expiration without a bounded node termination grace, surface that to its owner before deploying the permanent annotation; agree the maintenance/unblock procedure. Karpenter documentation warns that annotated pods can retain nodes. Do not conceal the scheduling/cost consequence or assume annotation protects every termination.
4. Obtain an aggregate pre-roll baseline through the existing t56 acceptance path: `/ready` 200, `storeMode: postgres`, account/user/machine counts 8/4/10 or an HQ-explained new baseline, no fresh `owner_unavailable`/token errors. No new provider refresh, billing or reset probe.

### Roll and proof

5. Merge the exact approved head through the installed helper only on HQ's deployment go; t56 observes Argo reconciliation of the tracked staging overlay. No manual application of staging-ha, forced pod deletion, drain, scale or restart.
6. Watch Ready counts and placement throughout: at least two Ready; each successor becomes Ready before a further pod is removed; final 3/3 Ready on distinct nodes in three zones. Zero surge avoids the planned fourth pod; two zones during a replacement is expected. Existing scheduler constraints alone do not prove three available zones, hence the explicit final placement check.
7. Inspect termination events during this natural rollout: no new `FailedPreStopHook` for the updated pods, no forced grace expiry, successful lease handover, and zero new account-server `owner_unavailable` or token errors. Distinguish old-pod failures (old hook) from new-pod failures; the initial roll terminates old-hook pods, so its events alone cannot prove the new hook works live.
8. Verify the new Pod-template annotation and effective strategy on every new pod/Deployment; PDB desired healthy 2/disruptions allowed at most 1 for the healthy three-pod steady state. Retain image digest and all frozen cutover resources.
9. Use the existing #194 acceptance pattern through t56 (public readiness/PostgreSQL mode, catalog parity, Mac + devbox normal client proof and HQ dashboard disposition). No duplicate Revoke or arbitrary new provider call is implied. For live confirmation of the replacement hook, capture its behavior at the next **authorized** natural termination; the local regression proves the hook ordering now. Do not schedule an unapproved destructive disruption test.
10. Per HQ's approval, t56 observes ten minutes without errors and completes the Mac turn; do not substitute a two-hour delay for that release gate. Longer recurrence observation can report consolidation-eviction counts and reason categories, with interruption/expiration/forceful terminations separate. Node failure, interruption, forced deletion and some bounded drift/expiration paths remain possible despite the annotation. Record any live lifecycle evidence still pending without claiming an unobserved termination passed.

### Stop and rollback

If Ready drops below two, public PostgreSQL readiness fails, a new token-error burst starts, the rollout stalls, or the rollout/initial success check has not passed by the agreed hard end: STOP and notify HQ/t56 immediately. Roll back by a normally reviewed revert of this PR via GitOps; t56 observes Argo and restoration. No window extension without Operator approval. A revert also rolls pods, so do not force-delete pods or bypass safety checks to hurry it. Do not revert H/I, repoint the database, run backfill, reactivate the old file writer or touch its PVC. Restoring the old configuration reintroduces its known hook/consolidation risks; record that explicitly in the rollback receipt.

## Open items for HQ/t56

- HQ approved delay-only preStop, annotation **and** tighter PDB, and integer `0/1` strategy as one PR.
- Metadata readbacks supplied by HQ at 14:0xZ as recorded above. The particular `Forceful Termination` cause and deleted-pod exit/drain completion remain unknown unless an executor has retained receipts.
- Deployment gate and t56 acceptance ownership confirmed by HQ; obtain the exact-head reviews/checks and coordinate the rollout start before merging.
