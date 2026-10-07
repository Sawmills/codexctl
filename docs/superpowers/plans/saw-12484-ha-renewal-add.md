# SAW-12484: HA login renewal and add-account plan

**Goal:** Make server-managed login renewal and add-account work through any PostgreSQL replica, with one refresh owner per server account.

**Architecture:** PostgreSQL owns encrypted login records, reservations, and recovery. Reuse the native device-login protocol and the account lease, epoch fencing, and settlement from #131/#133. File mode keeps its current persistence.

**Stack and basis:** Rust, Tokio, `tokio-postgres`, existing vault encryption; main `36b6711` (#137), [ADR 0002](../../adr/0002-postgresql-shared-state-for-central-ha.md), [add-account contract](central-add-account.md), [renewal contract](2026-10-01-central-relogin.md), and [SAW-12484](https://linear.app/sawmills/issue/SAW-12484).

## 1. Shared journal and admission

Extend `src/central/storage.rs` with a versioned schema migration and transactional login records: kind, company user, initiating machine ID, normalized alias, request ID, sequence, phase, deadline, cancel intent, landed alias, worker incarnation, settlement evidence, and encrypted candidate/commit payload. Preserve identity claims, old/new digests, terminal receipts, and unresolved reservations.

Enforce unique request identity and one active operation per company user/alias. Same-machine retries adopt the active ID; other machines receive a conflict. An operation lease has holder, monotonic epoch, and DB-clock expiry; add needs it before an account exists. Worker transitions check lease and sequence. Cancellation writes a separate authorized intent.

Serialize migration, renewal, and add admission with one transaction advisory lock and consistent lock order. Compare shared accounts/reservations using `relogin/inventory.rs` claim rules. Atomically reserve a new identity or link to the same company user's existing alias, retaining its label; cross-user ownership refuses. Keep transactions short, without external I/O. Check fresh DB time after lock waits; retain conservative renewal deadlines from #133.

## 2. One refresh owner and shared recovery

Adapt `relogin/{state,http,worker,recover,add}.rs` and `managed.rs`. All replicas share status/cancel. Only the operation lease holder runs the isolated login child. Publish spawn intent before launch, then encrypted candidate and exit evidence before admission; homes remain local execution state.

Renewal reserves the account, waits for its refresh owner to stop and commit credentials, then acquires its refresh lease. Add atomically creates an unverified account and transfers its reservation before taking that lease; existing-account landing follows renewal. Reuse `LeaseWindow`, `lease_guarded_verification`, and deferred settlement. Gate startup, token, background, and import launches on shared reservations. Promotion checks both fences and commits credentials, revision, and progress together. Complete after verification and durable settlement; local snapshots cannot overwrite shared revisions.

Cancel, revocation, deadline, or lease loss stops and awaits the child; revalidate machine authorization before admission. Lease loss terminates refresh RPCs and rejects stale writes. Expiry or a foreign PID lookup cannot prove exit or upstream completion. A durable in-flight marker blocks refresh until settlement. Late results enter incarnation-bound recovery receipts, never credential promotion without a current fence.

After a crash, resume shared settled candidates or committed revisions without repeating completed work. Lost local grants and unresolved calls remain unavailable with an explicit recovery reason. Unknown child identity retains a registry fence; known identity fences overlapping accounts. Keep #137's refusal for legacy file journals; recover shared journals. Preserve quarantine, refusal cleanup, and phase-sensitive cancellation.

## 3. Test-first delivery after HQ approval

Add regressions in `tests/central_ha_startup_test.rs`, storage tests, and `tests/fixtures/central_codex.py`; retain `tests/central_managed_test.rs` file-mode coverage. For each seam, prove failure, implement, then rerun:

- Two replicas with separate homes: renewal/add, cross-replica polling/cancel, concurrent IDs, machine revocation, same-user landing, cross-user refusal, and one refresh child during simultaneous token/import requests.
- Crash before spawn, after local grant save, after shared candidate commit, during landing/promotion, and after completion before HTTP response. Assert durable reservations, no duplicate account, and correct retry or explicit unresolved state.
- Lose either lease during login, initialization, verification, and settlement; delay old-worker results past takeover. Assert child termination, stale-write rejection, no second upstream refresh, and no revision regression.
- DB outage/reconnect, wrong-account quarantine, legacy-journal refusal, shared-journal restart, and unrelated-account availability where identity is known.

Use local `initdb`, `DATABASE_URL`, and `CODEXCTL_CENTRAL_DB_TLS=0`. Run `cargo fmt --all -- --check`, `cargo clippy --all-targets`, and `cargo test --all-targets --features central-real-db-tests`, then remaining CI gates. Update `docs/{central-server,central-ha}.md`; reuse bounded failure counters, structured logs, and `CodexctlCredentialOperationFailed`. Test counts/rules separately from notification delivery.

**HQ challenge:** Confirm the unresolved-process recovery boundary; grants lost with a replica cannot recover automatically. Commit/push this plan and stop at `PLAN READY SAW-12484 <sha>`. Implementation needs `HQ: plan approved`; PR handoff needs green CI, exact-head Architect approval, and Claude review. Brief fences remain in force.
