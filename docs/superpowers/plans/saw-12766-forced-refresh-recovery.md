# Forced refresh recovery implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Recover retryable file-mode refresh owners on demand and bound forced rotations to one per 60 seconds.

**Architecture:** Reuse the existing identity-gated background recovery and settlement path for a single requested account. Keep PostgreSQL recovery and permanent provider rejections fenced. Persist wall-clock rotation time in the encrypted vault at the owner snapshot boundary. The existing retry-clock test hook drives both clocks.

**Tech stack:** Rust, Tokio, Axum, existing native Codex fixture.

1. Update the default-background-off HTTP regression to require recovery at 5 seconds. Run it red. Reuse recovery for the requested owner with 5/10/20/40/60-second cooldowns and existing identity/settlement safeguards. Run it green and commit.
2. Add HTTP tests for repeated failures, cooldown cap/reset, and permanent rejection. Run each red where behavior changes, then implement and check.
3. Add a forced-request HTTP regression: two matching revisions produce one rotation; after 61 seconds another rotation is allowed. Run red, add the snapshot rotation timestamp and guard, then run green.
4. Add stderr and metrics assertions with synthetic credentials. Run red, implement bounded decision logs and counters, then run green.
5. Run formatting, Clippy, and all-target tests with the devbox PostgreSQL fixture. Review standards and ticket scope, run Claude autoreview, open the PR, request Architect review on the exact head, and merge after approval and green required checks. HQ owns release and deployment.

Interface choice: keeping machine context on the refresh owner would make it mutable shared state that each caller must clear. Instead, the token operation accepts an optional registered machine ID. Internal probes omit it; HTTP requests supply the authorized ID. Both interfaces retain the same token outcomes and settlement rules, but the explicit parameter prevents one request from logging another machine. Native work stays in detached, permit-held tasks after client cancellation.

HTTP error interface choice: returning a cooldown response through the token success result would make callers coordinate error accounting and status. An optional numeric Retry-After field on the existing error keeps the status, body, failure marker, and counter intact. Only request-driven retryable cooldowns set it; the owning refresh owner calculates the remaining seconds.

Billing policy choice: adding separate requested/cache booleans would let callers combine invalid states. A private BillingRead enum selects Omit, Live, or Recent. Only served_recent selects Recent, which reuses limits under 30 seconds old for the same revision and keeps their observation time. All modes retain routing verification and settlement; a stale or missing observation takes the existing live path.

## Part 2: suppress forced refresh of valid access tokens

The server deployment brief is `~/Code/.fleet/codexctl/briefs/server-storm-fix-deploy.md`. This PR covers step 1 only. HQ owns the later image, canary, pin, and deployment steps.

1. Add an HTTP regression in `tests/central_managed_test.rs`: a matching `previousRevision` for a token with days left keeps the revision after the 60-second guard expires, reuses fresh billing evidence, and emits `served_recent` with `reason=token_valid`. Run `cargo test --locked --test central_managed_test forced_request_serves_a_valid_token_without_rotating_or_reading_live_limits -- --exact` red.
2. In `src/central/server.rs`, check stored access-token expiry against `fast_path::MIN_REMAINING_SECONDS` before a machine-requested forced rotation. Reuse the existing `BillingRead::Recent` path. In `src/central/refresh_control.rs`, select a bounded reason for the decision. Run the regression green.
3. Add an HTTP case with less than one hour left. Prove one rotation and no additional rotations on retries; keep the 60-second guard and internal login/reset verification probes. Update existing rotation tests to use near-expiry credentials rather than healthy credentials. HQ confirmed that unknown expiry keeps the refresh path and 60-second guard; test missing and invalid expiry with `reason=exp_unknown` on attempted and guarded forces, counted in the existing metric.
4. Update `docs/central-server.md`. Run formatting, Clippy, default and no-default tests, PostgreSQL tests, and the release build. Review standards and scope, run Claude autoreview, and request t5D and Architect review on the exact PR head. Merge only after both reviews and required CI pass. Do not change images or deploy.

## Follow-up: unmarked native owner-read failures

HQ confirmed the initial incident shape: a valid grant and successful refresh
followed by three usage reads and an unmarked native error without a rejection
log. Pod stdout for the p3 failure at 03:04:16Z was lost at the 03:25Z restart.
The retained p3 Codex log at 04:50–05:00Z shows `account/read` 1969, then
`account/rateLimits/read` 1970 at 04:57:43Z, with no response, WARN, or ERROR;
central logged `owner_refresh/unclassified` at 04:57:47Z. That trace cannot
prove the RPC code, presence of `data.retryable`, or EOF versus timeout.
The protocol regressions cover both unmarked completed errors and EOF; the
short-deadline real-child test covers timeout without changing production waits.

1. Reproduce the file-mode failure through `/v1/token`, with background recovery
   off: two successful usage reads followed by an unmarked native failure. Assert
   the `owner_refresh/provider/unclassified` log, the cooldown, and recovery in
   the same server process. The baseline stays at 503 after the backoff.
2. Classify completed unmarked native owner-read errors as retryable. Keep the
   existing identity, journal, routing, settlement, and bounded-backoff checks.
   Explicit provider rejection must override even a native retry marker.
3. Cover account and routing reads, repeated failures, and provider rejection.
   Keep the RPC transport-failure predicate intact for EOF settlement; apply
   rejection precedence only to the owner-read retry decision. Prove with real
   PostgreSQL that rejection plus EOF publishes the final journal and releases
   the lease before a peer serves it.
   Run the devbox gates and Claude autoreview; request t5D and Architect review.
   HQ owns any image pin or deployment; keep the B33 and dashboard holds.

Interface choice: classifying only the native RPC error response would lose
stderr's bounded provider-rejection evidence or make the transport own the
account's fence policy. Instead, the refresh owner's existing failure-reporting
boundary passes the native error and its collected provider evidence to the
private RPC classification method before selecting the fence. Initialization
can report rejection without an owner-read error. No response payload or raw
provider error message is retained or logged.

Follow-up diagnostics: retain only a fixed failure class, optional numeric RPC
code, and presence of `data.retryable` at the RPC boundary. Include them only
on unclassified owner-refresh logs, clearing earlier metadata before each call.
Test native errors with hostile message/data keys, EOF recovery, and timeout
classification through real protocol children, and a broken stdin pipe after
child exit. No native message or payload
is retained. Compare recording metadata at the protocol boundary with parsing
error strings at the logger: boundary capture preserves numeric/presence facts
and distinguishes EOF from timeout without depending on untrusted text. It
adds no new retries, cancellation, or persistence behavior.
