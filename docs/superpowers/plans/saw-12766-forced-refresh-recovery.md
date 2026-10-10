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
