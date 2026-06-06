# codexctl

Manage multiple OpenAI Codex CLI accounts. Switch profiles, check rate limits across all accounts, and tab-complete profile names.

## Install

```bash
cargo install --git https://github.com/Sawmills/codexctl
```

## Usage

### Save accounts

Bootstrap each Codex account through `codexctl` so the Codex login runs in an isolated auth home:

```bash
codexctl login amir@example.com    # opens Codex device login, saves as profile

codexctl login amir+2@example.com
```

After an account is saved, switch with `codexctl use <alias>` instead of running `codex --login`
again. A fresh Codex login can invalidate another saved seat on the same ChatGPT account/workspace;
`codexctl login` avoids logging over `~/.codex/auth.json` by running Codex with
`CODEX_HOME=~/.codexctl/login-homes/<alias>`, and `codexctl use` only swaps the local auth file.

If you already logged in with Codex directly, save the current `~/.codex/auth.json`:

```bash
codexctl save work-main
```

### Check rate limits

```bash
codexctl status
```

```
Live status fetched at Tue Apr 28 22:20:56

Rate-Limited Accounts
┌──────────────────────┬─────┬───────────┬─────┬────────────────────────────────┬─────────────┐
│ Account              ┆ 5h  ┆ 5h Reset  ┆ 7d  ┆ 7d Reset                      ┆ Token       │
╞══════════════════════╪═════╪═══════════╪═════╪════════════════════════════════╪═════════════╡
│ amir+5@sawmills.ai   ┆ 0%  ┆ in 5h 00m ┆ 25% ┆ in 4d 18h (Sun May 03 16:20) ┆ 9d 23h      │
│ * amir@sawmills.ai   ┆ 78% ┆ in 3h 54m ┆ 92% ┆ in 4d 15h (Sun May 03 13:20) ┆ 3h 20m      │
│ amir+8@sawmills.ai   ┆ -   ┆ -         ┆ -   ┆ -                            ┆ invalidated │
└──────────────────────┴─────┴───────────┴─────┴────────────────────────────────┴─────────────┘

Usage-Based Accounts
┌───────────────────────────┬─────────┬──────┬─────────┬───────┬─────────┐
│ Account                   ┆ Balance ┆ Seat ┆ Credits ┆ Spend ┆ Token   │
╞═══════════════════════════╪═════════╪══════╪═════════╪═══════╪═════════╡
│ amir+ezra@sawmills.ai     ┆ -       ┆ -    ┆ ok      ┆ ok    ┆ 9d 1h   │
│ amir+reviewer@sawmills.ai ┆ -       ┆ -    ┆ ok      ┆ ok    ┆ expired │
└───────────────────────────┴─────────┴──────┴─────────┴───────┴─────────┘
```

Sorted by availability — most available accounts first. All accounts are fetched live in parallel.
The account column is the saved profile alias, with `*` marking the active account.

The `Token` column shows how long the stored access token is good for **without re-logging in**
(green = days left, yellow = hours, red = under an hour). An `invalidated` value means OpenAI revoked
the grant server-side even though the token has not yet timed out — this happens when another seat
on the same ChatGPT account is logged in, since a fresh `codex login` revokes the previously-active
seat. Prefer `codexctl use` (a pure file copy that never contacts OpenAI) over re-logging-in, and
only re-login a seat once its token genuinely shows `expired`.

Usage-based accounts are shown in a separate table with balance, seat limit, credits, and spend
control status.

### Switch accounts

Direct:

```bash
codexctl use amir+5@sawmills.ai
```

Interactive fuzzy picker:

```bash
codexctl switch
```

### Run Codex with spend-cap recovery

Use `codexctl codex` as the Codex launcher when you want account failover:

```bash
codexctl codex
codexctl codex -- "start prompt"
codexctl codex -- -C ~/Code/codexctl -m gpt-5
codexctl codex resume 019e8489-aa28-7071-ab90-16b81c7cfd1d
codexctl codex --allow-billing -- "start prompt"   # unattended: may use credits
```

The wrapper runs `codex` in a PTY and watches for this spend-cap message:

```text
You hit your spend cap set by the owner of your workspace. Ask an owner to increase your spend cap to continue.
```

Codex is launched from the current directory where `codexctl codex` was run. When detected, it
terminates that Codex process, switches to another account, then resumes with
`codex resume <session-id> "Continue the previous request."`. For a new session it discovers the
session id from the new Codex session file created under `~/.codex/sessions/`; for an existing
session, pass `resume <session-id>` so the wrapper can recover without discovery.

Account selection during recovery:

- **Never** switches to usage-based accounts (they bill credits).
- Auto-rotates only among rate-limited accounts that won't bill — spend cap reached (overage
  closed, so they hard-stop at 100% instead of drawing credits) with rate-limit headroom —
  preferring the most headroom, and moving to the next one each time the cap is re-hit.
- When only credit-billing accounts remain (spend cap not reached, so they draw credits past
  100%), it asks for confirmation before switching, and refuses on a non-interactive terminal.
  Pass `--allow-billing` to approve those switches without prompting (e.g. for unattended runs).

### Other commands

```bash
codexctl list          # list saved profiles
codexctl login <alias> # isolated Codex login and save
codexctl whoami        # show active account
codexctl codex -- ...  # run Codex with spend-cap recovery
codexctl remove <alias>
```

## Shell completions

```bash
# zsh (source-based)
codexctl completions zsh > ~/.cache/zsh/completions/_codexctl

# bash
codexctl completions bash >> ~/.bashrc

# fish
codexctl completions fish > ~/.config/fish/completions/codexctl.fish
```

Completions dynamically list profile names for `use` and `remove`.

## How it works

Profiles are stored in `~/.codexctl/profiles/<alias>/` — each containing a copy of `auth.json` and `meta.json`. `codexctl login <alias>` runs `codex login --device-auth` with an isolated `CODEX_HOME` under `~/.codexctl/login-homes/<alias>/`, imports that auth file, then switches to the saved profile. Switching copies the profile's `auth.json` into `~/.codex/auth.json`.

Rate limits are fetched from `chatgpt.com/backend-api/wham/usage` using the stored access tokens.
When an account ID is available, codexctl sends it as `chatgpt-account-id` so the usage response is
scoped to the intended account/workspace.

Supports both Codex CLI auth formats:

- Nested: `{"auth_mode": "chatgpt", "tokens": {"access_token": "..."}}`
- Flat: `{"access_token": "..."}`

## License

Apache-2.0
