# SAW-12454 plan: Amir's Claude accounts on the account server

Scope: Amir's own Claude subscription accounts only. No sharing, no loans, no other company user.
Base: codexctl origin/main d9e2fce; claudectl#15 head 6ad3401.
Status: plan approved with changes K1 to K8 (rule 57 challenge on 9ac0ea1, 2026-10-06). No code before Amir's design word (A6).

## Step 1 findings

- claudectl#15 needs no rebase. Its merge base is claudectl main 7b14c68 (2026-10-01), and main has no commit since. CI is green on 6ad3401 (Check, Trunk, Build macOS).
- codexctl#74 (closed 10-04) adds a separate `account-server` binary and image with a file-only Claude engine (`anthropic.rs`, `anthropic_http.rs`, `anthropic_login.rs`, `anthropic_usage.rs`, `providers.rs`). Its merge base is eaf8986 (10-02). Main changed about 2.8k lines in `managed.rs`, `enrollment.rs` and `vault.rs` since then, so its broker, enrollment and revoke edits do not apply.
- Main has no provider field (`vault::Vault`, `central_accounts`), no audit table, and no account revoke API. The SAW-12444 branch marks the loan hook at `src/central/loans/http.rs:356`.

Reuse map from main and #74:

| Need | Reuse | New work |
|---|---|---|
| SSO, machines | `enrollment.rs`, `Broker::authorize` (managed.rs:526), device revoke (managed.rs:2293) | Anthropic allow list per company user |
| Catalog | nothing from the Codex catalog (K1) | `anthropic_accounts` table and file namespace |
| Token issue | `authorize` again after a refresh (managed.rs:1389), the AES-GCM vault key (vault.rs:164) | Claude refresh engine from #74 (HTTP, not an app-server child) |
| Migration | #74 admission with `migration_id` receipts; claudectl#15 fence | admission on the Claude store |
| HA (B33) | none in this ticket (K5) | follow-up ticket |
| Audit | JSON operation lines on stderr and `/metrics` | one Claude audit line per operation (K8) |

## Design A (approved by HQ): Claude accounts inside the existing server, outside the Codex account path

- Store (K1): Claude accounts live in their own `anthropic_accounts` table and their own file namespace (`providers/anthropic/accounts/<digest>/vault.enc`, from #74), with their own lease rows. `central_accounts` and `vault::Vault` stay unchanged. No Codex code path reads Claude rows, so `hydrate_accounts` (managed.rs:2424), `prepare_owner` (2490), the owner scans (3038, 3044), relogin recovery (`inventory.rs:261`, `recover.rs:149`), backfill (storage.rs:513), `owner_record` (466) and `vault::account`/`validate_auth` (vault.rs:209, 231) never see one. The sealed grant uses the Codex vault key (vault.rs:164).
- No Anthropic token on an OpenAI path (K2): `account_catalog` (managed.rs:1475), `refresh_legacy_usage`/`fetch_direct` (1451), `/v1/accounts` and client recovery and auto-select (native.rs:1153), `account_summary`, the dashboard (dashboard.rs:147) and the resets list read only the Codex store. One test per site proves that a Claude account is absent.
- Gate: the Claude routes need the server flag `--providers openai,anthropic` and a company user in `anthropic_users` from the SSO config (Amir only). Any other user gets 403 `provider_not_enabled`.
- Store mode (K5): in PostgreSQL or Dual mode the Claude routes return 503 `anthropic_unavailable`. Staging runs File mode for the pilot. B33 support for Claude is a follow-up ticket.
- Token issue to claudectl: claudectl#15 keeps its contract. `POST /v2/anthropic/token {account_id, previous_revision}` returns an access token, expiry, revision and identity, never a refresh token. claudectl `server run` writes the token into a private settings file for one pinned Claude process and renews it before expiry.
- Renewal handoff gate (K3): claudectl writes `settings.json` `env.CLAUDE_CODE_OAUTH_TOKEN` (central_session.rs:108) and sets an invalid process environment token (central_session.rs:124). Before any migration, a synthetic or throwaway session must run across a token change and the evidence must show which value Claude Code uses. If the settings value does not win, the migration stops.
- Refresh ownership and rotation: the server is the only refresh owner, under a per-account mutex and the Claude lease row. The server refreshes when `previous_revision` matches or when less than 5 minutes remain (K4, not #74's 60 s). claudectl renews early, before that margin. Refresh tokens are single use: after admission the server makes one forced refresh and an identity check, and logs a digest-only record of whether the token rotated. The phase machine (Ready, Refreshing persisted before the request, Unverified, Ready after the `/api/oauth/profile` identity check) stays. The server never replays a lost refresh response; the account then needs login renewal.
- Revoke: device revoke and user disable work unchanged, because every Claude route calls `authorize` and checks again after a refresh. New `DELETE /v2/anthropic/accounts/{id}` removes the sealed grant, and token requests then return 410. The server cannot recall an access token it already gave out. On revoke the server stops renewal and the claudectl lease ends. The maximum exposure is one token lifetime; the pilot measures that lifetime and the docs record it.
- Loan exclusion (K6): loans read only the Codex store, so the separate table excludes Claude accounts by structure. SAW-12444 still adds a `provider_not_supported` test at `loans/http.rs:356`.
- Audit (K8): one structured line per migrate, issue, refresh and revoke, with operation, device id, account digest and result. No token or grant appears in a line.
- Deploy: the normal codexctl release and the existing staging overlay. No new image, ECR path or k8s access.
- Risk: Claude code runs in the Codex process. Mitigation: the Claude engine has its own task and timeouts, and the provider flag defaults to `openai`.

## Design B: the standalone #74 server (not chosen)

- Same API and engine, as a separate `account-server` image on #74's file vault and a process lock, so one replica only and no PostgreSQL mode.
- Revoke: #74 has device revoke only. Account revoke is new work, as in A.
- Deploy: a new image, ECR repository and k8s workload. The lane has no platform access for that (the claudectl#15 parked blocker).
- Cost: a rebase across 2.8k changed lines, a second deploy, and two servers to run. Benefit: Claude failures cannot touch Codex token issue.

## Refresh-holder inventory and fence (before the pilot migration)

The pilot is one inactive Claude account, never the active one.

1. Inventory every holder of the pilot grant (K7), by grant digest with a tool that never prints a token:
   - claudectl profiles and the `~/.claudectl/run-*` session directories on each machine;
   - the Keychain on both Macs;
   - every `~/.claude/.credentials.json` copy on the devbox;
   - the claudectl usage cache;
   - the Claude capacity guard list, headless `claude -p` jobs, cron jobs and lane panes pinned to the account.
2. Remove the pilot from the capacity guard list and confirm the removal. The guard code refuses any alias with a server marker.
3. HQ approves the inventory. A holder of unknown status stops the migration. Backups (Time Machine and similar) stay a known residual risk.
4. Run `claudectl server migrate --exclusive-owner` on one machine. The claudectl fence then blocks a restore of that grant or identity there.
5. On every other holder, delete the copy by digest match and record a fence there too.

## Tests (test first, synthetic tokens only)

- Server engine: port #74's six engine tests to the Claude store (one refresh for concurrent callers, no replay of a lost response, admission retry, successor verification, login retry, cached usage). Add the forced refresh after admission and its rotation log line.
- Isolation (K1, K2): Codex startup with Claude rows present; one test per OpenAI site (catalog, legacy usage, `/v1/accounts`, recovery and auto-select, summary, dashboard, resets) that shows no Claude account; `/v1/token` with a Claude account id returns 404.
- Gate and mode: non-Amir user gets 403; PostgreSQL and Dual mode return 503 `anthropic_unavailable` (K5).
- Margin (K4): an end-to-end test with expiry given in seconds and in milliseconds proves that the server refreshes inside 5 minutes and not before.
- Revoke: account revoke returns 410; a device revoked during a refresh gets no token; a revoked device gets no new token.
- Audit (K8): each operation writes one line, and no line contains a token.
- Loans (K6): SAW-12444 test for `provider_not_supported`.
- claudectl: the existing offline status and lost-receipt tests, token renewal across expiry against a fake server, local `use` with no network, and the K3 renewal handoff check.
- Live pilot after deploy (HQ word only): one migration, a Mac and a devbox session across one renewal, revoke, and a record of rotation and token lifetime.
