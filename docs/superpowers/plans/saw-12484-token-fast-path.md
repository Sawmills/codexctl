# Plan: Serve a Valid Token Without the Lease (SAW-12484 Follow-up, B33 Blocker)

**Decision.** In PostgreSQL mode, any pod serves a committed access token from PostgreSQL without the account lease and without a native child when four conditions hold: the token expires at least 5 minutes from now; the request does not force a refresh; fresh billing and routing evidence for that exact credential revision exists in PostgreSQL; and no login fence covers the account. Every other request takes today's lease path. A request that meets the lease of another pod polls the account revision until the holder commits, then serves the new revision. This replaces option (a) as the B33 blocker fix; (a)'s bounded lease wait survives only as the loser's last step (see [Lease losers](#lease-losers)).

Reader: the implementing lane and the HQ that approves it (rule 57). Facts cite `main` at `c7fd94e`.

## Why

Today every PostgreSQL token request takes the account lease, starts a native owner child, calls `account/read` (and `account/rateLimits/read` when `billing` is set), stops the child, and releases the lease (`src/central/managed.rs` token handler; `src/central/server.rs` `Owner::tokens` and `with_billing`). A second request for that account on another pod gets `503 refresh_in_progress` at once, and the client does not retry it (`src/central/native.rs`). Access tokens live for days, so almost every request needs no refresh; the lease and the child cost seconds and cause the 503.

## Token Request Today

| Response field                                   | Source                                                                               |
| ------------------------------------------------ | ------------------------------------------------------------------------------------ |
| `access_token`, `chatgpt_account_id`, `revision` | Stored auth: `revision` is the digest of the auth JSON (`Owner::snapshot`)           |
| `chatgpt_plan_type`                              | Access-token claim; overwritten from `account/rateLimits/read` when `billing` is set |
| `billing_class`, `statusline_usage`              | `account/rateLimits/read` through the child, only when `billing` is set              |
| `native_routing_supported`                       | `account/read` routing check through the child, every request                        |
| `label`                                          | Vault                                                                                |

`previous_revision` rule (`Owner::tokens`): absent means no forced refresh; different from the current revision means "serve current, no refresh"; equal to the current revision means "the client saw this token rejected, force a refresh".

## Design

### 1. Evidence cache (schema version 8)

New table `central_token_evidence`, one row per account:

```sql
CREATE TABLE central_token_evidence (
    account_id  TEXT PRIMARY KEY REFERENCES central_accounts(account_id) ON DELETE CASCADE,
    revision    TEXT NOT NULL,          -- auth digest the evidence was observed on
    billing_class TEXT,                 -- NULL when billing was not read
    plan_type   TEXT,
    limits      JSONB,                  -- raw rateLimits response for statusline usage
    routing_supported BOOLEAN NOT NULL, -- account/read routing verdict
    observed_at TIMESTAMPTZ NOT NULL
);
```

The lease path writes it under the lease, in the same fenced write that publishes the credential, after a successful `with_billing` whose `revision` equals the published revision. A routing refusal writes `routing_supported=false`. Any credential write with a new revision makes the row stale by definition (revision mismatch), so no delete is needed.

### 2. Fast path conditions

The fast path runs before the owner lock and the lease, after alias and loan resolution. It reads the account record and its evidence row in one query that also evaluates the login fence (the same `NOT EXISTS` predicate that the lease claim uses for login journals and identity reservations). It serves only if every rule holds:

1. **Expiry:** `exp` of the access token (`api::token_expiry`) is at least 300 s after the database clock.
2. **Forced refresh:** `previous_revision` is absent or differs from the current revision. Equal means forced: lease path.
3. **Revision binding:** evidence `revision` equals the current revision.
4. **Routing:** `routing_supported` is true. A refusal or a missing row goes to the lease path, which owns the refusal error.
5. **Billing freshness (billing safety):** when `billing` is set, `billing_class` is present and `observed_at` is at most 60 s old. When `billing` is not set, routing evidence may be up to 10 minutes old. Stale evidence goes to the lease path, which re-reads rate limits. The 60 s bound matches the account catalog interval, so a usage-based switch is seen no later than the catalog sees it.
6. **Fences:** no login fence, the account record is verified, and the local owner is not fenced (`available`, `!routing_refused`). A pod with a local fence uses the lease path, which owns recovery.
7. **Identity:** the stored alias matches the requested alias (rename check), and for a borrowed reference the existing loan checks run unchanged after the fast path, as they do after the lease path.

The response carries `statusline_usage` built from cached `limits` with `age_seconds` from `observed_at`. Post-processing after the inner block stays the same for both paths: `refresh_legacy_usage`, re-authorization, loan confirmation and issue, activity, and the live-session write.

### 3. Lease losers

When the lease path cannot claim the lease because another holder's lease is live, the request does not fail at once. It polls the account record every 50 ms to 1 s with jitter, for up to 15 s:

- If the revision changes, the holder committed: re-evaluate the fast path on the new revision and serve if it passes.
- If the holder's lease is released without a new revision (a non-rotating read), claim the lease and continue on the lease path. This is the only part of option (a) that remains.
- If 15 s pass, return `503 refresh_in_progress` as today.

A login fence or an expired, unsettled lease still refuses at once.

### 4. Metrics

- `codexctl_central_token_fast_path_total{result="served|expiring|forced|evidence_missing|evidence_stale|routing|fenced"}`
- `codexctl_central_lease_waits_total{outcome="committed|acquired|timeout|refused"}` and `codexctl_central_lease_wait_seconds_sum` and `_count`
- Existing `codexctl_central_failed_requests_total{reason="owner_unavailable"}` must not rise in the two-pod tests.

### 5. Not changed

File mode keeps its single pod and its in-process owner lock. The lease path, its fences, settlement, and refresh-owner rules do not change. Enrollment, resets, and loans keep their current paths.

## Tests (test-first, devbox PostgreSQL)

1. **Two pods, valid token:** pod A holds the lease with a held child; pod B serves the same account in under 1 s with `launches()==0` on B and a `served` fast-path count.
2. **Two parallel requests on two pods:** both 200, one child launch in total, no `owner_unavailable`.
3. **Forced refresh:** `previous_revision` equal to current goes to the lease path (one launch, `forced` count).
4. **Expiring token:** `exp` less than 300 s away goes to the lease path (`expiring`).
5. **Billing freshness:** evidence older than 60 s with `billing:true` goes to the lease path and rewrites evidence; a usage-based class observed on the lease path is what the next fast path returns.
6. **Revision binding:** a credential write with a new revision makes the next request take the lease path (`evidence_stale`).
7. **Login fence:** an account under a relogin fence is never served by the fast path (`fenced`).
8. **Loser polls the revision:** pod A holds the lease and commits a rotation; pod B's forced request returns A's new revision with `launches()==0` on B and a `committed` wait count.
9. **Loser claims after a non-rotating holder:** pod B claims the lease once A releases it without a new revision (`acquired`).
10. Existing HA and managed tests stay green; the HA test that asserts an immediate `refresh_in_progress` from other pods changes to the waiting behavior.

## Delivery

One PR on SAW-12484: migration to version 8, the evidence write on the lease path, the fast path, the loser loop, metrics, `docs/central-ha.md` updates, and the tests above. Commits test-first; devbox full gates on a fresh database; Codex autoreview (Claude fallback recorded); Architect APPROVED on the exact head; then a new `Central server image` digest for the B33 runbook. The B33 runbook prerequisite 7 then points at this PR, and window step 7 measures fast-path hit rate and p99 latency.

## Risks

- **Stale billing evidence:** bounded at 60 s for `billing:true`; a stricter bound costs more lease-path calls. HQ may tighten it.
- **Routing drift:** an account can lose native routing between checks. Bound: 10 minutes without `billing`, 60 s with it; a refused request on the client already triggers `previous_revision`, which forces the lease path.
- **Schema version 8:** the B33 migrate Job gains one table; the emptiness query already covers new tables.
