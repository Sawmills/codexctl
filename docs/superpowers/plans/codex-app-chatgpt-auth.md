# Plan: Codex Desktop App on Central Accounts with ChatGPT Features (Option A)

**Decision.** The desktop app uses the built-in `openai` provider. codexctl writes `~/.codex/auth.json` in `"auth_mode": "chatgptAuthTokens"` mode from the central server's `TokenResponse`, with no refresh token, and a launchd agent rewrites it before the access token expires. The central server stays the only refresh owner. Research: `~/Code/.fleet/codexctl/research-codex-central-provider.md` (Codex `f73a478`).

## Command

`codexctl app-auth enable [--account <alias>] [--allow-billing] | refresh | status | disable`

- `enable` checks preconditions, backs up, pins one account, writes the file, sets the provider, and loads the agent. `refresh` is what the agent runs. `status` prints the account, token expiry, last refresh result, Codex version, and agent state, never a token. `disable` restores the config and removes our file.
- Account: default is the central account `codexctl use` selected; `--account` names another. The alias is pinned at `enable`. `refresh` never switches accounts, because Codex treats an account change on reload as a mismatch error. A pinned account that runs out stays pinned; `status` says so, and `enable --account <other>` plus an app restart moves it.
- Billing: reuse `finish_token`'s rule. A token that is not `RateLimited` is refused without `--allow-billing` and a matching approval; non-interactive runs refuse.

## Writer

- File shape, matching Codex `AuthDotJson::from_external_access_token`: `auth_mode`, `OPENAI_API_KEY: null`, `tokens {id_token: <access JWT>, access_token, refresh_token: "", account_id}`, `last_refresh`. Validate the token's account against the pinned connection first (`validate_token_account`).
- Claims (M5): parse the access JWT before any write; require `chatgpt_plan_type` and an account id that equals the pin, else refuse.
- Missing file (M3): the app's logout deletes `auth.json`; `refresh` then stops, records the reason, and never writes a new one.
- Atomic write: temp file in `~/.codex`, mode 0600, fsync, rename. Write only when the current file is ours (`auth_mode == chatgptAuthTokens` and the account matches the pin) or during `enable` after the backup. A foreign file (the user logged in again) stops `refresh` with an error and no write.
- Store check: refuse when `cli_auth_credentials_store` is `keyring` or `auto` (macOS `auto` uses the Keychain and bypasses the file). Absent or `file` passes.
- Provider: `enable` removes `model_provider = "codexctl-central"` from `~/.codex/config.toml` with a `toml_edit` edit. It refuses while the central provider is active through `codexctl use` unless the operator deactivates first, so one `~/.codex` never has two modes. `codexctl codex` without `CODEX_HOME` falls back to `~/.codex` (`src/commands/codex.rs:1184-1188`), so while app-auth is enabled a marker under `~/.codexctl/app-auth/` makes `use`, central activation, and `codex` recovery refuse to touch `~/.codex`. Plain `codex` runs on that home use provider `openai` and the pinned account. Enable it only on the MacBook, never on Mac mini or devbox lane homes (M2).

## Refresher

- launchd agent `~/Library/LaunchAgents/ai.sawmills.codexctl.app-auth.plist`, `StartInterval` 300 s plus `RunAtLoad`, runs `codexctl app-auth refresh`. `StartInterval` does not fire during sleep, so the run after wake checks expiry first (M4).
- This mode has no proactive refresh: Codex rotates only through a 401 and a file reload, so expect one 401 per rotation (M6). `refresh` asks the server for a token with at least agent interval plus 15 min of life left (M4), skips a token that equals the one on disk, and rewrites when less than 30 min is left or the server revision changed.
- A failure keeps the old file and writes `~/.codexctl/app-auth/state.json` (last success, last error reason, failure count). `status` reads it. The log goes to `~/.codexctl/logs/app-auth.log` without tokens.

## Restore

`enable` copies `auth.json` and `config.toml` to `~/.codexctl/app-auth/backup/` (0600) once, with SHA-256 values. `disable` unloads and removes the agent, restores `config.toml`, and removes our `auth.json`. It restores the backed-up login only when that login's account id is not in the central catalog, or with `--restore-login` (M1): a server account's refresh token on disk would be a second refresh owner. If `config.toml` changed since `enable`, `disable` refuses without `--force`.

## Version pin

Record the Codex version that passes LIVE-A (CLI `codex --version`, and the app's bundled binary). `enable` and `refresh` refuse other versions unless `--allow-codex-version` is set, because the mode is marked internal and unstable.

## Live test first (LIVE-A, MacBook, before the agent is built)

Back up both files; write one file through a small `codexctl app-auth enable --no-agent` build; Amir restarts the app; check the JWT plan claim, Meetings, apps, web search, one prompt. Then replace the file with an expired token and confirm that Codex reloads the file on a 401 and does not call `auth.openai.com/oauth/token`. That 401 reload path is unverified at runtime and decides the design. Restore on FAIL.

## Tests (test-first, `tests/app_auth_test.rs` plus unit tests)

`auth_mode` exactly `"chatgptAuthTokens"` (M6); exact file shape and 0600; JWT without plan or account refused (M5); missing `auth.json` stops refresh with no write (M3); token with too little life refused, unchanged token skipped (M4); `use`, activation, and recovery refused while enabled (M2); backup login of a server account not restored by default (M1); atomic replace; keyring and auto refused; central provider active refused; usage-billed refused without `--allow-billing`; refresh skips a fresh token and rewrites a near-expiry one; foreign file stops refresh with no write; token account mismatch refused; disable restores both files byte for byte and removes the agent; changed files refused without `--force`; unsupported Codex version refused; refresh failure keeps the old file and records the reason; no token on stdout or in the log.
