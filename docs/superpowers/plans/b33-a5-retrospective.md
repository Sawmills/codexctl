# B33 attempt 5 retrospective and window checks

## Outcome (2026-10-15)

HQ accepted the PostgreSQL HA cutover at **06:26Z**. The final backfill took
27 seconds and matched the stopped file source: **8 accounts, 4 users and
10 devices**, with the other compared inventories at zero. Three HA pods were
Ready across three zones; per-pod readiness reported PostgreSQL. Public
`/ready` returned 200 with PostgreSQL mode at 06:26:15Z. HQ reported successful
Mac and devbox status checks and the Mac pinned turn `ha ok`.

The accepted sequence was H wave 0 (file writer to zero), wave 1 (final backfill,
which migrates first), wave 2 (HA startup), H acceptance, then I (traffic switch).
There was no separate W or initial migrate/backfill run in this sequence.
[H #167](https://github.com/Sawmills/codexctl/pull/167) landed as `59193def`
at 05:25:05Z; [I #168](https://github.com/Sawmills/codexctl/pull/168) landed
as `0db09ca2` at 06:20:19Z. The token outage was approximately 05:26–06:26Z,
within the 07:20Z hard end.

The old file writer remains at zero; the original `state-codexctl-0` PVC is
retained. Attempt 5 used the fresh `codexctl_a5` database and preserved the old
database. [Reset draft #189](https://github.com/Sawmills/codexctl/pull/189)
was unused and is closed as superseded. Infra rollback
[#1613](https://github.com/Sawmills/infra/pull/1613) stays held.

These are attributed HQ/Mac-executor receipts, recorded on
[SAW-12484](https://linear.app/sawmills/issue/SAW-12484) and in
the fleet questions file; the devbox lane did not execute the cluster proofs.
This record grants no new merge, Job, reset, rollback or deployment authority.
Attempt 5's temporary scan exception ended when the attempt closed. Historical
commands and flags in [section 5](../../central-ha.md#5-staging-migration-and-rollback)
and the [attempt-3 runbook](b33-attempt-3-cutover.md) require a newly reviewed
plan and authorization before reuse.

## Findings and required checks

Use these checks when preparing a future central-service window. Record each
result and its owner in that window's checklist before writer stop. Preserve
section 5's WINDOW CLOCK, phased rollback and credential-settlement gates.

1. **Keep the driver independent.** Codex lanes lost authentication during
   the outage, and HQ/Claude had to drive I. Assign a non-codexctl driver or a
   pre-provisioned local-auth executor whose model access, shell and merge
   credentials remain usable while central and its relay route are unavailable.
   Before GO, verify that execution path without making a live outage or moving
   credentials. Record the driver, cluster executor and fallback. A running
   lane that still obtains its tokens from central does not satisfy this gate.

2. **Prepare I immediately after H lands.** H's squash left I stale. HQ rebased
   I, obtained fresh Architect approval at 05:56Z, and waited for CI to finish
   at 06:19Z; the extra CI cost roughly 20 minutes during the outage. Immediately
   after H merges, rebase I onto the resulting main, inspect its remaining diff,
   and obtain the required approvals and green checks on the new exact head.
   Record that head and the landed H SHA. Keep I unmerged until H acceptance
   passes and HQ gives GO-I; an earlier approval does not cover the rebase.

3. **Set ownership before the window.** Merge attempts were refused twice
   because PR ownership metadata was missing. Before GO, set and read back the
   ticket association and required lane-ownership labels on every execution PR,
   including the held rollback PR. Verify the assigned owner can use the
   installed merge helper and the approved App identity. Treat any refusal as
   a stop for that merge and report it; new attempts need the applicable owner
   instruction. A review verdict alone does not establish merge authority.

4. **Check relay upgrade compatibility before restart.** The v0.1.52 relay
   refused v0.1.51's same-user `0755` shared `~/.codexctl/relay/` directory,
   causing about a minute of Mac relay downtime. Verify the installed version
   and directory ownership/modes against [the relay upgrade instructions](../../relay.md#run-it).
   Operators remaining on v0.1.52 use its documented one-time permission repair;
   v0.1.53 includes [#202's compatibility fix](https://github.com/Sawmills/codexctl/pull/202).
   Keep the private identity directory and key protections. Under the approved
   host rollout, restart the upgraded service before new routed launches, then
   require `codexctl relay status` to prove listener identity. Save the result
   before declaring the host ready.

5. **Retry a failed log read once.** During [#204's accepted roll](https://github.com/Sawmills/codexctl/pull/204#issuecomment-6195745516),
   t56's watcher stopped on a log-read failure; HQ supplied the remaining
   observation. Watchers must save the failed read's time and exit category,
   then retry that same bounded, read-only log request once. If it fails again,
   stop the watcher, report the observation gap, and obtain a named replacement
   observer before claiming acceptance. Do not interpret a failed read as zero
   service errors. This retry applies only to observation, never to a provider
   request, copy stream, mutation or failed CI job.

## Follow-up boundary

This change records checks in the operator's runbook; it adds no automation or
watcher code. The watcher implementation remains with its executor owner.
Temporary scan-exception removal and the earlier deferred Job hardenings remain
separate follow-ups; closing a5 does not complete them. Future windows need
their own reviewed image, readiness receipts, owner GO, planned end and success
check. Preserve the PVC and held rollback until their owners authorize otherwise.
