# SAW-9895 codexctl hardening plan

## Outcome

Make account status accurate for the current multi-bucket subscription contract.
Prevent unsafe profile paths and billable automatic selection.
Make auth-file changes private, serialized, and atomic per file.
Restore dependency and release gates.

## Verified inputs

- Homebrew and `/opt/homebrew/bin/codexctl` both use version `0.1.16`.
- The live main bucket currently has one 604,800-second window.
- The live response also has a named `GPT-5.3-Codex-Spark` bucket.
- The current Codex app-server returns the same `codex` and `codex_bengalfox` buckets.
- Current OpenAI pricing docs define a shared rolling 5-hour limit and possible weekly limits.
- The CLI must render the server response. It must not invent a missing 5-hour value.
- Claude Opus reviewed the architecture. It confirmed the findings and the scope limits below.

## Work

1. Add centralized profile-alias validation and checked profile-path construction.
   Validate CLI input, stored metadata, active aliases, and server-derived aliases.
   Reject absolute paths, separators, parent components, control characters, non-ASCII names, hidden staging names, case-fold collisions, and excessive length.
   Keep existing email aliases valid.

2. Add centralized billing classification in `src/api.rs`.
   Use `RateLimited`, `UsageBased`, and `Unknown` states.
   Allow automatic selection and recovery only for confirmed rate-limited accounts.
   Keep usage-based and unknown accounts out of all no-bill automatic paths.

3. Add a store lock and atomic file primitives.
   Lock profile mutations and live-auth switches.
   Write a temporary file in the destination directory, sync it, set private Unix permissions, then rename it.
   Order a switch so the live auth file is installed before the active marker changes.
   Accept that a crash between the two atomic files leaves the old marker.
   In that state, never attribute the new live auth to the old alias; a repeated explicit switch reconciles both files.
   Preserve the existing active-profile token-refresh ownership checks.

4. Use the live auth file for the active profile.
   Centralize auth-source selection.
   Use saved snapshots only for inactive profiles or when the live token subject does not belong to the selected profile.

5. Add bounded HTTP clients.
   Use explicit connect and total request timeouts for blocking and async calls.
   Reuse one async client within each multi-account command.

6. Update the status model for the current contract.
   Parse all returned named rate-limit buckets.
   Classify windows by their declared duration.
   Render only window classes returned by the server.
   Show additional named buckets without exposing auth data.
   Keep reset-credit and token-expiry data aligned with the main account bucket.

7. Remove state creation from information-only commands.
   Parse the command before store initialization.
   Do not create `.codexctl` for help, version, completions, or invalid commands.

8. Update rejected dependencies.
   Upgrade `anyhow` past RUSTSEC-2026-0190.
   Upgrade or remove the locked `quinn-proto` version rejected by GHSA-4w2j-m93h-cj5j.
   Enforce a RustSec lockfile scan in `.github/workflows/ci.yml` and confirm the compiled dependency tree.

9. Harden `.github/workflows/release.yml`.
   Use least-privilege job permissions.
   Pin third-party actions to full commit SHAs.
   Pin the GitHub App private-key consumer.
   Make a Homebrew update failure fail the workflow.
   Refuse to replace published assets with different bytes.
   Verify the tag and Cargo version match.

10. Add regression coverage.
    Cover path traversal, invalid stored aliases, active marker validation, billing classification, unknown-account exclusion, active auth ownership, atomic permissions, timeouts, adaptive columns, multiple buckets, and no-state CLI paths.

## Gates

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`
- `cargo run -- --help`
- RustSec `Dependency Audit` CI job
- `trunk check`
- Read-only Claude Opus architecture review of the implementation plan and critical invariants
- Exact worktree, branch, HEAD, and dirty-state proof
- Installed-path verification after a release and Homebrew installation

## Scope limits

- Do not refactor `src/commands/codex.rs` in this change.
- Do not replace all command path wrappers with dependency injection.
- Do not claim crash consistency across several files without a journal.
- Do not add Windows locking or permission claims when the shipped targets are macOS and Linux.
- Do not push, open a pull request, merge, release, or modify the Homebrew tap without explicit authority.
