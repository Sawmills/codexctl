# SAW-12454 plan: Amir's Claude accounts on the account server

Scope: Amir's own Claude subscription accounts only. No sharing, no loans, no other company user.
Base: codexctl origin/main d9e2fce; claudectl#15 head 6ad3401.
Status: plan only. No code before `HQ: plan approved, design <A|B>`.

## Step 1 findings

- claudectl#15 needs no rebase. Its merge base is claudectl main 7b14c68 (2026-10-01), and main has no commit since. CI is green on 6ad3401 (Check, Trunk, Build macOS).
- codexctl#74 (closed 10-04) adds a separate `account-server` binary and image with a file-only Claude engine (`anthropic.rs`, `anthropic_http.rs`, `anthropic_login.rs`, `anthropic_usage.rs`, `providers.rs`). Its merge base is eaf8986 (10-02). Main changed about 2.8k lines in `managed.rs`, `enrollment.rs` and `vault.rs` since then, so its broker, enrollment and revoke edits do not apply.
- Main has no provider field (`vault::Vault`, `central_accounts`), no audit table, and no account revoke API. The SAW-12444 branch marks the loan hook at `src/central/loans/http.rs:356`.

Reuse map from main and #74:

| Need | Reuse | New work |
|---|---|---|
| SSO, machines | `enrollment.rs`, `Broker::authorize` (managed.rs:526), device revoke (managed.rs:2293) | Anthropic allow list per company user |
| Catalog | `central_accounts`, `vault::Vault` | `provider` column and field, default `openai` |
| Token issue | per-account `Mutex`, `acquire_lease` (storage.rs:704), `fenced_write`, second `authorize` after refresh (managed.rs:1389) | Claude refresh engine from #74 (HTTP, not an app-server child) |
| Migration | #74 admission with `migration_id` receipts; claudectl#15 fence | port admission onto `CentralStore` |
| HA (B33) | `CentralStore` File/Postgres/Dual, `account_refresh_leases` | none beyond the provider column |
| Audit | JSON operation lines on stderr and `/metrics` | Claude operations use the same lines and counters |

## Design A (HQ recommends): provider type inside the existing server

- Catalog: add `provider` to `vault::Vault` (serde default `openai`) and to `central_accounts` (schema version 2, `DEFAULT 'openai'`). Claude accounts use key `account_key(user, "anthropic/" + alias)`, so the OpenAI keys stay unchanged and one alias can exist once per provider. ADR 0004 records "a server account has one provider"; CONTEXT.md widens "Server account".
- Gate: the Claude routes need the server flag `--providers openai,anthropic` and a company user in `anthropic_users` from the SSO config (Amir only). Any other user gets 403 `provider_not_enabled`. `/v1/token` refuses a Claude account and `/v2/anthropic/token` refuses an OpenAI account.
- Token issue to claudectl: claudectl#15 keeps its contract. `POST /v2/anthropic/token {account_id, previous_revision}` returns an access token, expiry, revision and identity, never a refresh token. claudectl `server run` writes the token into a private settings file for one pinned Claude process and renews it before expiry.
- Refresh ownership and rotation: the server is the only refresh owner. The #74 engine runs under the per-account mutex and the PostgreSQL refresh lease and saves through `fenced_write`. A refresh runs when `previous_revision` matches or expiry is under 60 s. The phase machine (Ready, Refreshing persisted before the request, Unverified, Ready after the `/api/oauth/profile` identity check) stays. A cut-off exchange is never replayed and the account needs login renewal. The engine accepts a rotated or a kept refresh token, because Anthropic rotation is not measured; the pilot measures it.
- Revoke: device revoke and user disable work unchanged, because every Claude route calls `authorize` and checks again after a refresh. New `DELETE /v2/anthropic/accounts/{id}` sets `deleted_at`, removes the sealed grant, and makes token requests return 410. A delivered access token stays valid until it expires; the server cannot recall it.
- PostgreSQL mode (B33): Claude rows ride `central_accounts` and `account_refresh_leases` with no new table. Dual mode mirrors them to the file store like OpenAI rows.
- Loan exclusion: SAW-12444 refuses `provider != openai` at `loans/http.rs:356` (lend) and again at `grant_owner` (token time). Whichever ticket merges second adds the check and its test.
- Deploy: the normal codexctl release and the existing staging overlay. No new image, ECR path or k8s access. The image needs no Node or Claude binary.
- Cost: about 1.5k lines server side (engine port, routes, provider column, gate), claudectl#15 nearly unchanged. Risk: Claude code in the OpenAI process; a Claude bug can take down Codex token issue. Mitigation: the engine has its own task and timeouts, and the provider flag defaults to `openai`.

## Design B: the standalone #74 server

- Same API and engine, as a separate `account-server` image.
- Token issue, refresh and rotation as in A, but on #74's file vault (`providers/anthropic/accounts/<digest>/vault.enc`) and a process lock, so one replica only.
- PostgreSQL mode: none. B33 HA needs a second port of the engine to `CentralStore`.
- Revoke: #74 has device revoke only; account revoke is new work, as in A.
- Loan exclusion: not needed while B has no loans, but the shared code still needs the SAW-12444 check if the servers merge later.
- Deploy: a new image, ECR repository and k8s workload. That needs platform access the lane does not have (the claudectl#15 parked blocker).
- Cost: a rebase across 2.8k changed lines, a second deploy, and two servers to run. Benefit: Claude failures cannot touch Codex token issue.

Recommendation: A. It reuses SSO, machines, HA and deploy, and the risk has a cheap mitigation.

## Refresh-holder inventory and fence (both designs)

The pilot is one inactive Claude account, never the active one. Before migration:

1. Inventory every holder of the pilot grant: claudectl profiles and the live credential on the Mac (Keychain) and the devbox (`~/.claude`), the Claude capacity guard rotation list, headless `claude -p` jobs, cron jobs, and lane panes pinned to that account. Compare by grant digest with a tool that never prints the token.
2. Fence: take the pilot out of the capacity guard list, confirm no pane or job uses it, and run `claudectl server migrate --exclusive-owner` on one machine. The claudectl fence then blocks any restore of that grant or identity on that machine.
3. On every other holder, delete the copy by digest match and record a fence there too.
4. HQ approves the inventory before step 2. A holder of unknown status stops the migration.

## Tests (test first, synthetic tokens only)

- Server: port #74's six engine tests onto `CentralStore` (single refresh for concurrent callers, no replay of a lost response, admission retry, successor verification, login retry, cached usage). Add: provider gate (non-Amir user 403), `/v1/token` refuses Claude and `/v2/anthropic/token` refuses OpenAI, account revoke returns 410, device revoked during a refresh gets no token, PostgreSQL lease epoch fences a second replica (`central-real-db-tests`), dual-mode mirror, schema 1 to 2 migration keeps OpenAI rows.
- Loans: lend and borrow of a Claude account return 409 `provider_not_supported`.
- claudectl: the existing offline status and lost-receipt tests, plus token renewal across expiry against a fake server, and local `use` with no network.
- Live pilot after deploy (HQ word only): one migration, a Mac and a devbox session across one renewal, revoke, and a record of rotation behavior.
