# SAW-12484: HA login renewal and add-account plan

**Goal:** Make server-managed login renewal and add-account work through any PostgreSQL replica, with one refresh owner per server account.

**Architecture:** PostgreSQL owns encrypted login records, reservations, and recovery. Reuse the native device-login protocol and the account lease, epoch fencing, and settlement from #131/#133. File mode keeps its current persistence.

**Stack and basis:** Rust, Tokio, `tokio-postgres`, existing vault encryption; main `36b6711` (#137), [ADR 0002](../../adr/0002-postgresql-shared-state-for-central-ha.md), [add-account contract](central-add-account.md), [renewal contract](2026-10-01-central-relogin.md), and [SAW-12484](https://linear.app/sawmills/issue/SAW-12484).

## 1. Shared journal and admission

Extend `src/central/storage.rs` with a versioned schema migration and transactional login records: kind, company user, initiating machine ID, normalized alias, request ID, sequence, phase, deadline, cancel intent, landed alias, worker incarnation, settlement evidence, and encrypted candidate/commit payload. Preserve identity claims, old/new digests, terminal receipts, and unresolved reservations.

P2-4: After authorization, look up the request ID in PostgreSQL before availability, alias, or lease checks. A lost completion response retried on another replica returns its receipt without another login. Enforce unique request identity and one active operation per company user/alias. Same-machine retries adopt the active ID; other machines receive a conflict. An operation lease has holder, monotonic epoch, and DB-clock expiry; add needs it before an account exists. Worker transitions check lease and sequence. Cancellation writes a separate authorized intent.

Serialize migration, renewal, and add admission with one transaction advisory lock and consistent lock order. Compare shared accounts/reservations using `relogin/inventory.rs` claim rules. Atomically reserve a new identity or link to the same company user's existing alias, retaining its label; cross-user ownership refuses. P2-3 (PR2): Back namespaced identity claims with a database unique index; a second login matching an in-flight add reservation refuses with `relogin_reserved`. Test that race. Keep transactions short, without external I/O. Check fresh DB time after lock waits; retain conservative renewal deadlines from #133.

## 2. One refresh owner and shared recovery

Adapt `relogin/{state,http,worker,recover,add}.rs` and `managed.rs`. All replicas share status/cancel. Only the operation lease holder runs the isolated login child. Publish spawn intent before launch, then encrypted candidate and exit evidence before admission; homes remain local execution state.

P1-2: The operation lease alone covers device polling. Keep the existing account usable for the 900-second login deadline. Only after the candidate is committed to PostgreSQL, reserve the account, wait for its refresh owner to stop and commit credentials, then acquire its refresh lease. Add atomically creates an unverified account and transfers its reservation before taking that lease; existing-account landing follows renewal. Reuse `LeaseWindow`, `lease_guarded_verification`, and deferred settlement. Gate startup, token, background, and import launches on shared reservations. Promotion checks both fences and commits credentials, revision, and progress together. Complete after verification and durable settlement; local snapshots cannot overwrite shared revisions.

P3-8: Poll durable cancellation at most every second; fail closed on a bounded database read failure, including A losing database access while B cancels. Cancel, revocation, deadline, or lease loss stops and awaits the child; revalidate machine authorization before admission. Lease loss terminates refresh RPCs and rejects stale writes. Expiry or a foreign PID lookup cannot prove exit or upstream completion. A durable in-flight marker blocks refresh until settlement. Late results enter incarnation-bound recovery receipts, never credential promotion without a current fence.

P1-1/P3-9: Bind login children to parent death and enforce a local lease watchdog. For device polling, an expired operation lease and unrenewed holder incarnation permit `replica_lost`, reservation release, and a new request ID. Test killing A during the device code and retrying on B. This rule does not apply to verification: P2-6 requires a durable in-flight marker before any refresh-capable launch; recovery of that marker is unresolved and never repeats verification. P2-5: `storage::backfill` refuses `add::pending_logins`; legacy file journals never enter the shared table. Preserve quarantine, refusal cleanup, and phase-sensitive cancellation.

## 3. Approved delivery split (P2-7)

1. **PR1 now:** schema, encrypted shared journal, operation lease, cross-replica status/cancel, and renewal only. Include request receipt ordering, candidate-before-account-lease ordering, backfill refusal, and verification in-flight fencing. Add routes stay unavailable.
2. **PR2:** add-account admission, unique identity reservations, and same-user landing.
3. **PR3:** crash/takeover recovery, including expired device-login cleanup and killed-holder retry. PR1 retains unresolved operations for this recovery rather than claiming takeover complete.

## 4. Test-first validation

Add regressions in `tests/central_ha_startup_test.rs`, storage tests, and `tests/fixtures/central_codex.py`; retain `tests/central_managed_test.rs` file-mode coverage. For each seam, prove failure, implement, then rerun:

- Two replicas with separate homes: renewal/add, cross-replica polling/cancel, concurrent IDs, machine revocation, same-user landing, cross-user refusal, and one refresh child during simultaneous token/import requests.
- Crash before spawn, after local grant save, after shared candidate commit, during landing/promotion, and after completion before HTTP response. Assert durable reservations, no duplicate account, and correct retry or explicit unresolved state.
- Lose either lease during login, initialization, verification, and settlement; delay old-worker results past takeover. Assert child termination, stale-write rejection, no second upstream refresh, and no revision regression.
- DB outage/reconnect, wrong-account quarantine, legacy-journal refusal, shared-journal restart, and unrelated-account availability where identity is known.

Use local `initdb`, `DATABASE_URL`, and `CODEXCTL_CENTRAL_DB_TLS=0`. Run `cargo fmt --all -- --check`, `cargo clippy --all-targets`, and `cargo test --all-targets --features central-real-db-tests`, then remaining CI gates. Update `docs/{central-server,central-ha}.md`; reuse bounded failure counters, structured logs, and `CodexctlCredentialOperationFailed`. Test counts/rules separately from notification delivery.

**Approval:** HQ approved with changes P1-1 through P3-9 on 2026-10-07. No further plan round. Implement PR1, obtain Claude review, open its PR, and stop at `needs re-review <pr> <sha> CI green`. HQ owns the subsequent exact-head Architect challenge. Brief fences remain in force.
