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

Profile aliases use printable ASCII and are unique without regard to letter case. This keeps the
credential-store namespace identical on default macOS and Linux filesystems.

### Check rate limits

```bash
codexctl status
```

```
Live status fetched at Tue Apr 28 22:20:56

Rate-Limited Accounts
┌──────────────────────┬───────────────────────┬─────┬──────────────────────────────┬────────┬────────┐
│ Account              ┆ Limit                 ┆ 7d  ┆ 7d Reset                     ┆ Resets ┆ Token  │
╞══════════════════════╪═══════════════════════╪═════╪══════════════════════════════╪════════╪════════╡
│ * amir@sawmills.ai   ┆ Codex                 ┆ 13% ┆ in 6d 23h (Fri Aug 07 15:31) ┆ -      ┆ 4d 19h │
│ * amir@sawmills.ai   ┆ GPT-5.3-Codex-Spark  ┆ 0%  ┆ in 6d 23h (Fri Aug 07 16:28) ┆ -      ┆ -      │
│ amir+2@sawmills.ai   ┆ Codex                 ┆ 100%┆ in 6d 17h (Fri Aug 07 09:29) ┆ 1      ┆ 4d 19h │
│ amir+2@sawmills.ai   ┆ GPT-5.3-Codex-Spark  ┆ 0%  ┆ in 6d 23h (Fri Aug 07 16:28) ┆ -      ┆ -      │
└──────────────────────┴───────────────────────┴─────┴──────────────────────────────┴────────┴────────┘

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

Rate-limit windows are matched by their server-declared duration. A 5-hour or 7-day column appears
only when at least one returned bucket contains that window. Named model or feature buckets appear
as separate rows. `codexctl` does not show an empty 5-hour column when the service returns only a
weekly window.

Automatic account selection uses the main `Codex` bucket. Additional buckets are model-specific or
feature-specific status. They do not block general account selection without a reliable mapping
from the requested model or feature to that bucket.

The `Resets` column shows banked rate-limit resets (see [Banked resets](#banked-resets)):
`3 (2 now)` means three are held and two can be redeemed this second; a bare count turns red when
a credit lapses within three days.

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

- **Never** switches to usage-based or unknown-billing accounts.
- Auto-rotates only among rate-limited accounts that won't bill — spend cap reached (overage
  closed, so they hard-stop at 100% instead of drawing credits) with rate-limit headroom —
  preferring the soonest-resetting seat by default (see Reset-aware selection), and moving to the
  next one each time the cap is re-hit.
- When only credit-billing accounts remain (spend cap not reached, so they draw credits past
  100%), it asks for confirmation before switching, and refuses on a non-interactive terminal.
  Pass `--allow-billing` to approve those switches without prompting (e.g. for unattended runs).

### Reset-aware selection (default)

Both `codexctl use` (no alias) and `codexctl codex` recovery prefer, among otherwise-eligible
accounts, the one whose **7d window resets soonest**. This drains near-reset seats first and keeps
fresher seats in reserve, de-synchronizing the fleet so capacity refreshes gradually instead of
filling and resetting in a single cluster (which would otherwise leave the whole fleet dry for a
stretch before the cluster refreshes). Every other guarantee is unchanged: usage-based accounts are
never auto-selected, exhausted windows are skipped, and no-bill accounts win over credit-billing
ones (reset is only a tiebreak within a bill class).

This is the default. To opt out and restore the legacy most-headroom-first pick:

```bash
CODEXCTL_SELECT=most-available codexctl codex -- "..."
export CODEXCTL_SELECT=most-available   # alias: headroom / legacy
```

### Banked resets

OpenAI grants **banked rate-limit resets**: credits that clear an exhausted usage window on demand
instead of waiting for it to lapse. They are per-account, expire ~30 days after they are granted,
and are not refundable — so codexctl treats them as scarce.

```bash
codexctl resets                     # what every account holds, and when it expires
codexctl reset                      # redeem one for the active account
codexctl reset amir+5@sawmills.ai   # ...or for a specific one
codexctl reset --yes                # skip the confirmation (unattended)
codexctl resets --claim             # redeem everything about to lapse
codexctl resets --claim --within-days 7 --yes
```

```text
┌─────────────────────┬────────┬────────────┬────────────────────────┐
│ Account             ┆ Banked ┆ Redeemable ┆ Expiries               │
╞═════════════════════╪════════╪════════════╪════════════════════════╡
│ amir+p3@sawmills.ai ┆ 2      ┆ 2          ┆ Jul 31, Aug 12         │
│ * amir@sawmills.ai  ┆ 3      ┆ 0          ┆ Jul 26, Jul 31, Aug 12 │
└─────────────────────┴────────┴────────────┴────────────────────────┘
```

A reset only clears an _already-exhausted_ window — the backend reports zero redeemable credits
until an account actually hits 100%, and codexctl refuses to redeem before that rather than waste
one. When several credits qualify, it always spends the one closest to expiring.

`--claim` sweeps the whole fleet and redeems credits that are about to lapse (default: within three
days) on accounts that are already at 100%. Those credits are the ones with nothing left to lose:
the account cannot be used right now anyway, and the credit is about to evaporate. Note the flip
side — a credit on an account that is _not_ yet at 100% cannot be rescued at all, since the backend
will not apply a reset to a window that has nothing to clear.

### Reset-aware recovery

Both `codexctl use` (no alias) and `codexctl codex` recovery pick accounts from one cost-ranked
ladder, cheapest option first:

1. A no-bill account that still has rate-limit headroom — used silently, as before.
2. A banked reset whose credit would **expire before its window resets anyway** — redeemed without
   prompting, since holding it back cannot pay off.
3. A banked reset worth keeping — asks for confirmation, or pass `--allow-resets`.
4. A credit-billing account — asks for confirmation, or pass `--allow-billing`.

Resets rank ahead of credit-billing accounts because they cost no money. `--allow-billing` does
**not** imply permission to spend resets; each is approved separately.

```bash
codexctl use --allow-resets                                       # unattended: may spend resets
codexctl use --allow-billing                                      # unattended: may spend credits
codexctl codex --allow-resets -- "start prompt"
codexctl codex --allow-resets --allow-billing -- "start prompt"   # ...and may spend credits
```

So when every account is exhausted, `codexctl use` redeems a reset and hands back an account that
actually works, instead of a seat sitting at 100%. Passing an explicit alias never redeems — use
`codexctl reset <alias>` to spend a credit on a named account.

### Other commands

```bash
codexctl list          # list saved profiles
codexctl login <alias> # isolated Codex login and save
codexctl whoami        # show active account
codexctl codex -- ...  # run Codex with spend-cap recovery
codexctl resets        # list banked rate-limit resets
codexctl reset [alias] # redeem a banked reset
codexctl remove <alias>
codexctl --version     # installed version
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

Profiles are stored in `~/.codexctl/profiles/<alias>/` — each containing a copy of `auth.json` and `meta.json`. `codexctl login <alias>` runs `codex login --device-auth` with a unique isolated `CODEX_HOME` under `~/.codexctl/login-homes/<alias>/`, imports that auth file, removes the temporary login home, then switches to the saved profile. Switching copies the profile's `auth.json` into `~/.codex/auth.json`.

Rate limits are fetched from `chatgpt.com/backend-api/wham/usage` using the stored access tokens.
When an account ID is available, codexctl sends it as `chatgpt-account-id` so the usage response is
scoped to the intended account/workspace. Windows are matched by their declared duration rather
than by position, since plans that publish only a weekly limit return it in the `primary_window`
slot.

Banked resets use `wham/rate-limit-reset-credits` to list credits and
`wham/rate-limit-reset-credits/consume` to redeem one. Redemptions carry a client-generated
idempotency key, so retrying a timed-out request never spends a second credit.

Supports both Codex CLI auth formats:

- Nested: `{"auth_mode": "chatgpt", "tokens": {"access_token": "..."}}`
- Flat: `{"access_token": "..."}`

## License

Apache-2.0
