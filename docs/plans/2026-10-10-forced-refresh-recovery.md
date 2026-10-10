# Forced refresh recovery implementation plan

**Goal:** Recover retryable file-mode refresh owners on demand and bound forced rotations to one per 60 seconds.

**Architecture:** Reuse the existing identity-gated background recovery and settlement path for a single requested account. Keep PostgreSQL recovery and permanent provider rejections fenced. Persist wall-clock rotation time in the encrypted vault at the owner snapshot boundary. The existing retry-clock test hook drives both clocks.

**Tech stack:** Rust, Tokio, Axum, existing native Codex fixture.

1. Update the default-background-off HTTP regression to require recovery at 5 seconds. Run it red. Reuse recovery for the requested owner with 5/10/20/40/60-second cooldowns and existing identity/settlement safeguards. Run it green and commit.
2. Add HTTP tests for repeated failures, cooldown cap/reset, and permanent rejection. Run each red where behavior changes, then implement and check.
3. Add a forced-request HTTP regression: two matching revisions produce one rotation; after 61 seconds another rotation is allowed. Run red, add the snapshot rotation timestamp and guard, then run green.
4. Add stderr and metrics assertions with synthetic credentials. Run red, implement bounded decision logs and counters, then run green.
5. Run formatting, Clippy, and all-target tests with the devbox PostgreSQL fixture. Review standards and ticket scope, run Claude autoreview, open the PR, request Architect review on the exact head, and merge after approval and green required checks. HQ owns release and deployment.

Interface choice: keeping machine context on the refresh owner would make it mutable shared state that each caller must clear. Instead, the token operation accepts an optional registered machine ID. Internal probes omit it; HTTP requests supply the authorized ID. Both interfaces retain the same token outcomes and settlement rules, but the explicit parameter prevents one request from logging another machine. Native work stays in detached, permit-held tasks after client cancellation.
