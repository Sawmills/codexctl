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
`CODEX_HOME=~/.codexctl/login-homes/<alias>/session-*`, and `codexctl use` only swaps the local auth file.

If you already logged in with Codex directly, save the current `~/.codex/auth.json`:

```bash
codexctl save work-main
```

Profile aliases use printable ASCII, hold at most 128 bytes, and are unique without regard to
letter case. An alias cannot start with `.` or contain control characters, `/`, or `\`. This keeps
the credential-store namespace identical on default macOS and Linux filesystems.

### Check rate limits

```bash
codexctl status
```

Example from before weekly pace support:

```
Live status fetched at Tue Apr 28 22:20:56

Rate-Limited Accounts
┌──────────────────────┬───────────────────────┬─────┬──────────────────────────────┬────────┬────────┐
│ Account              ┆ Limit                 ┆ 7d  ┆ 7d Reset                     ┆ Resets ┆ Token  │
╞══════════════════════╪═══════════════════════╪═════╪══════════════════════════════╪════════╪════════╡
│ * amir@sawmills.ai   ┆ Codex                 ┆ 13% ┆ in 6d 23h (Fri Aug 07 15:31) ┆ -      ┆ 4d 19h │
│                      ┆ GPT-5.3-Codex-Spark  ┆ 0%  ┆ in 6d 23h (Fri Aug 07 16:28) ┆        ┆        │
│ amir+2@sawmills.ai   ┆ Codex                 ┆ 100%┆ in 6d 17h (Fri Aug 07 09:29) ┆ 1      ┆ 4d 19h │
│                      ┆ GPT-5.3-Codex-Spark  ┆ 0%  ┆ in 6d 23h (Fri Aug 07 16:28) ┆        ┆        │
└──────────────────────┴───────────────────────┴─────┴──────────────────────────────┴────────┴────────┘

Usage-Based Accounts
┌───────────────────────────┬────────────────┬──────┬─────────┬───────┬─────────┐
│ Account                   ┆ Credit balance ┆ Seat ┆ Credits ┆ Spend ┆ Token   │
╞═══════════════════════════╪════════════════╪══════╪═════════╪═══════╪═════════╡
│ amir+ezra@sawmills.ai     ┆ -              ┆ -    ┆ ok      ┆ ok    ┆ 9d 1h   │
│ amir+reviewer@sawmills.ai ┆ -              ┆ -    ┆ ok      ┆ ok    ┆ expired │
└───────────────────────────┴────────────────┴──────┴─────────┴───────┴─────────┘
```

Sorted by availability — most available accounts first. All accounts are fetched live in parallel.
The account column is the saved profile alias, with `*` marking the active account.

Current status adds a `Pace` column when fresh weekly window data is available.
`+12 ahead` means usage is faster than the straight line through that window;
`-8 behind` means usage is slower. The `Fleet` row or footer shows the mean of known
pace across the displayed accounts. `-` marks unknown pace, and empty pace columns
are hidden. See [Account Status JSON](docs/status-json.md) for the calculation and
version 2 fields.

Rate-limit windows are matched and labeled by their server-declared duration. For example, the
table can show `15m`, `1h`, `5h`, or `7d` columns. A column appears only when at least one returned
bucket contains that duration. Each account has one table row. Named model or feature buckets use
aligned lines inside that row. `codexctl` does not invent a 5-hour window when the service returns
only a weekly window.

OpenAI's current [subscription documentation](https://learn.chatgpt.com/docs/pricing) describes one
shared agentic usage and credit pool, with plan and model-specific allowances. It does not promise
one universal window layout. The live [multi-bucket rate-limit
response](https://developers.openai.com/codex/app-server#6-rate-limits-chatgpt) is therefore
authoritative for the columns that `codexctl` shows.

Automatic account selection uses the main `Codex` bucket. Additional buckets are model-specific or
feature-specific status. They do not block general account selection without a reliable mapping
from the requested model or feature to that bucket.

The `Resets` column shows banked rate-limit resets (see [Banked resets](#banked-resets)):
`3 (2 now)` means three are held and two can be redeemed this second; a bare count turns red when
a credit lapses within three days.

The `Credits` column shows OpenAI credit balances and availability. Balances use
credits, for example `53,306.16 credits`, with two decimal places and comma separators.
These values are not dollars. The column appears when an account reports credits data,
including a zero balance. `status --json`
and the server dashboard expose `has_credits`, `unlimited`, `balance`, and
`overage_limit_reached`. Missing or null credits data stays absent in JSON.

The `Token` column shows how long the stored access token is good for **without re-logging in**
(green = days left, yellow = hours, red = under an hour). An `invalidated` value means OpenAI revoked
the grant server-side even though the token has not yet timed out — this happens when another seat
on the same ChatGPT account is logged in, since a fresh `codex login` revokes the previously-active
seat. Prefer `codexctl use` (a pure file copy that never contacts OpenAI) over re-logging-in, and
only re-login a seat once its token genuinely shows `expired`.

Usage-based accounts are shown in a separate table with balance, seat limit, credits, and spend
control status. The `Credit balance` column uses credits. The `Seat` column uses dollars.

When billing classification is unknown, status still shows the returned usage windows, resets,
and token expiry. A `Billing` column marks those accounts as `unknown`. An unrecognized plan or
missing main usage windows can cause this classification. Automatic selection still excludes
these accounts.

The `Plan` column shows the subscription returned by OpenAI, including Pro, Pro (More), and
Pro (Max). Subscription windows and purchased credits can coexist. Credits do not make a known
subscription unknown; spend control still determines billing risk during recovery. Plan names
are not invoices or credit balances in dollars.

### Terminal status bars

Run `codexctl statusline` for cached active-account usage such as `p2 62% wk · 6d22h`.
See [the statusline guide](docs/statusline.md) for cache refresh and tmux, Starship, zsh, and HerdR examples.

### Switch accounts

Direct:

```bash
codexctl use amir+5@sawmills.ai
```

Interactive fuzzy picker:

```bash
codexctl switch
```

### Codex app-server daemon

With `features.daemon_auto_start`, every `codex` TUI attaches to one shared app-server daemon.
The daemon reads `auth.json` once at startup and ignores a later swap, so a switch alone leaves
every session on the old account. After a switch, `codexctl use` and `codexctl switch` ask the
daemon which account it runs. If it runs another account, they offer to restart it:

```bash
codexctl use amir+5@sawmills.ai                    # asks before it restarts the daemon
codexctl use amir+5@sawmills.ai --restart-daemon   # restarts without asking
```

A restart stops the turns the daemon runs. Running turns get up to 60 s to finish first. After the
restart, codexctl sends `Continue the previous request.` to each session that was running or had
stopped on the usage limit. Those turns run with no sandbox and no approval prompts
(`approvalPolicy=never`, `sandbox=danger-full-access`). The restart question lists the sessions
and states these permissions before you answer, and every restart prints them before it resumes
anything, including `--restart-daemon` and `codexctl codex` recovery. A session whose turn finished during the restart receives no continuation prompt. Subagent sessions are left to their parent.

Before it continues a stopped session, `codexctl` updates the thread permissions and checks
that full access is active.
The daemon sends the changed settings to attached TUIs so they retain those permissions.
If the update fails, `codexctl` reports that session as not resumed and sends no continuation prompt.
Once applied, these permissions remain active even if the turn finishes or a later recovery step fails.
This path requires a Codex daemon that supports `thread/settings/update` (tested with Codex 0.158.0).

Open TUIs reconnect to the new daemon by themselves. Only sessions the daemon has loaded are
resumed: the daemon unloads an idle session about a second after its last client leaves, so a
session whose TUI was closed stays stopped.

Without a terminal and without `--restart-daemon`, `use` only warns. `codexctl whoami` warns when
the daemon account differs from the active profile. `codexctl codex` recovery restarts the daemon
without asking, because every session on it shares the exhausted account. It resumes the other
sessions; its own session relaunches with the flags it was started with. A pinned `exec` home
shares the daemon of `~/.codex`, so a switch inside a pinned lane never restarts it.

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

When the account server is active, `codexctl codex` spreads a new launch across
included accounts. It chooses the account with the fewest live token sessions,
then the lowest recent 429 rate. Rate collection has a five-second timeout; if it
times out, selection continues without 429 data. Usage-based accounts and exhausted windows are
never selected automatically. The selected account is pinned to this child, so
starting a new lane does not move existing sessions on the machine. If Codex stops after `exceeded retry limit, last
status: 429`, codexctl resumes the same session on the next least-loaded included
account, up to three recoveries per hour. A recovery keeps the original model,
permissions, working directory, and other arguments. Automatic selection and
429 recovery never redeem resets or select credit-billing accounts, even with
`--allow-billing` or `--allow-resets`. Explicit `--account` keeps its billing policy.
If no fresh included launch can be prepared, codexctl warns and launches the current
account through the existing provider. This fallback keeps that account's existing
approval; it does not grant new billing approval or redeem a reset.

### Pinned launches

For one server-account lane, use `codexctl codex --account <alias> [codex args...]`,
including `resume <session-id>`. It preserves the host account and keeps this session
pinned when other lanes switch. Billing accounts require terminal approval or
`--allow-billing`. An exhausted account requires `--allow-billing` to run on ChatGPT credits; no reset is redeemed.
See [server-account launches](docs/central-server.md) for policy and exit codes.

`codexctl rate [--json] [--minutes 10]` reports successful responses, 429s, their ratio,
observed processes/PIDs, and weekly usage from read-only Codex logs and account status.
Uncertain attribution has its own row. Token-per-minute data is not available.

`codexctl exec` supports saved local profiles only.

`codexctl use` changes the account for the whole machine. When two agent lanes start at the same
time, the second `use` can take the first lane's account before it launches. `codexctl exec` pins
credentials to one child process instead, and never touches `~/.codex/auth.json` or the active
marker:

```bash
codexctl exec --account amir+2@sawmills.ai -- codex -m gpt-5 "start prompt"
codexctl exec --account amir+2@sawmills.ai -- codexctl codex -- "start prompt"
```

`exec` owns `CODEX_HOME`, so it refuses to run when one is already set rather than replacing it
silently. Unset it first, or launch the pinned command from a shell that never exported it.

The second form composes pinning with spend-cap recovery: the wrapper reads its account from
`CODEX_HOME`, so its recovery switches stay inside the pinned home and remain invisible to every
other lane. `exec` also names the pinned account to its children in `CODEXCTL_PINNED_ALIAS`, so
recovery knows which account just failed and never switches straight back to it. `codexctl whoami` still reports whatever `codexctl use` last selected.

Each alias gets a persistent pinned home at `~/.codexctl/exec-homes/<alias>/`.
Credentials, daemon state, SQLite databases, locks, and unknown entries stay local to that home.
Only these entries from `~/.codex` are shared through symbolic links: `config.toml`, `AGENTS.md`,
`AGENTS.override.md`, `instructions.md`, `sessions/`, `archived_sessions/`, `session_index.jsonl`,
`history.jsonl`, `skills/`, `prompts/`, `rules/`, and `memories/`.
Session rollouts still land in `~/.codex/sessions/`, where `codex resume` finds them.
On launch, `exec` removes old links at excluded names only when they point into the live Codex
home. It preserves real files, directories, and links outside that home.

A refreshed token is folded back into the saved profile when the child exits.
An older token never overwrites a newer one. If Codex replaces a shared link with a real file,
that copy stops tracking the shared one. Delete `~/.codexctl/exec-homes/<alias>` to start clean.
The child's exit code becomes codexctl's.

On Windows, creating these links requires Developer Mode, the
`SeCreateSymbolicLinkPrivilege` user right, or an elevated `codexctl` process. If Windows denies
the operation because of missing link privileges, `exec` reports these remedies.

Install the standalone CLI on Windows with
`cargo install --git https://github.com/Sawmills/codexctl --no-default-features`.
The central prototype requires Unix process and shell support.
Windows tests require the same symbolic-link permissions as `exec`.
CI enables Developer Mode and tests both file and directory links before the suite.
Windows daemon synchronization has only mock-daemon test coverage.
Compatibility with a real Windows Codex daemon remains unverified.

### Reset-aware selection (default)

Both `codexctl use` (no alias) and `codexctl codex` recovery prefer, among otherwise-eligible
accounts, the one whose **7d window resets soonest**. This drains near-reset seats first and keeps
fresher seats in reserve, de-synchronizing the fleet so capacity refreshes gradually instead of
filling and resetting in a single cluster (which would otherwise leave the whole fleet dry for a
stretch before the cluster refreshes). Every other guarantee is unchanged: usage-based accounts are
never auto-selected, exhausted windows are skipped, and no-bill accounts win over credit-billing
ones (reset is only a tiebreak within a bill class).
Within each billing class, automatic selection skips any candidate with a reported usage window at
or above 95% whenever another candidate in that class is below 95%; this applies to both local and
server-account selection.
Thus a server seat at 97% is skipped in favor of a 10% seat even when the 97% seat resets sooner.

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
4. A credit-billing account.

Resets rank ahead of credit-billing accounts because they cost no money. `codexctl use` warns when
the selected account can use paid credits, then continues without confirmation. It still asks
before it redeems a banked reset. The `codexctl codex` active-session recovery path keeps the
stronger billing confirmation because that path can resume work and spend credits immediately.
Its `--allow-billing` flag does **not** imply permission to spend resets; each is approved
separately.

```bash
codexctl use --allow-resets                                       # unattended: may spend resets
codexctl codex --allow-resets -- "start prompt"
codexctl codex --allow-resets --allow-billing -- "start prompt"   # ...and may spend credits
```

So when every account is exhausted, `codexctl use` redeems a reset only after confirmation, with
`--allow-resets`, or when the reset would otherwise lapse before the natural window reset. It then
hands back an account that works instead of a seat at 100%. Passing an explicit alias never
redeems — use `codexctl reset <alias>` to spend a credit on a named account.

### Other commands

```bash
codexctl list                 # list saved profiles
codexctl login <alias>        # isolated Codex login and save
codexctl whoami               # show active account
codexctl label <alias> [text] # name an account (omit text to clear)
codexctl codex -- ...         # run Codex with spend-cap recovery
codexctl codex --account ALIAS resume SESSION # pin one server-account lane
codexctl rate --json          # read per-account response/429 counts
codexctl exec --account <alias> -- <command>  # pinned, non-mutating launch
codexctl resets               # list banked rate-limit resets
codexctl reset [alias]        # redeem a banked reset
codexctl remove <alias>
codexctl --version            # installed version
```

## Two accounts on one email

A personal account and a workspace seat can share one address, so the email
cannot tell them apart. Give each profile its own alias and a label:

```bash
codexctl login --label personal amir-personal
codexctl login --label team     amir-team
```

Or keep using the address and let the label separate the seats. When the alias
you asked for already holds a _different_ account, the label qualifies it
instead of overwriting:

```bash
codexctl login amir@sawmills.ai --label personal   # saves 'amir@sawmills.ai'
codexctl login amir@sawmills.ai --label team       # saves 'amir@sawmills.ai+team'
```

Logging the same seat in again refreshes whichever alias it already occupies,
so this is stable across re-logins — including when the alias you type is not
that one. An account lives in exactly one profile, and that profile is where a
login for it lands, whatever the alias asked for turned out to hold. The name
typed is not recorded as the address of a profile it did not name.

Without `--label` there is nothing to qualify with, so a login onto an alias
held by _another_ account, for an account not saved anywhere yet, is refused
rather than allowed to replace those credentials.

`codexctl save` refuses instead of redirecting. A login has already
authenticated by the time the alias is resolved, so refusing it would throw away
a working credential; a save has nothing to lose by asking you to name the alias
it would have written to.

`--label` also works on `codexctl save`, and `codexctl label <alias> [text]`
sets or clears one later.

```
$ codexctl list
┌──────────────────┬──────────┬──────────┬──────────────────┬────────┐
│ Account          ┆ Label    ┆ Plan     ┆ Email            ┆ Active │
╞══════════════════╪══════════╪══════════╪══════════════════╪════════╡
│ amir-personal    ┆ personal ┆ pro      ┆ amir@sawmills.ai ┆        │
│ amir-team        ┆ team     ┆ business ┆ amir@sawmills.ai ┆ *      │
└──────────────────┴──────────┴──────────┴──────────────────┴────────┘
```

The label is display text. The alias stays the only selector for `use`,
`remove`, and `reset`; `codexctl switch` also matches the label as you type.
The `Label` column appears in `list` and `status` only once some profile has
one.

`codexctl save` without an alias defaults to the detected email. When that
profile already holds a _different_ workspace, the save is refused rather than
offering an overwrite prompt that would replace the other account's tokens:

```
$ codexctl save
error: profile 'amir@sawmills.ai' holds a different account
       (stored workspace 033569a0…, incoming 6df34c28…).
       Pass an explicit alias: codexctl save <alias>
```

## Upgrading an existing store

An account is identified by its workspace and its login together — neither
alone, because a workspace holds many people and one login holds seats in many
workspaces. Profiles saved before this release recorded neither, so `codexctl`
reads them out of the stored token and remembers what it finds.

Two credentials that positively disagree are never the same account, and
`codexctl` refuses to overwrite one with the other. A profile that declares
_nothing_ is a different situation: the store cannot tell whether the account
arriving is the one it holds, but the operator who just logged in can. Those
cases ask rather than refuse — on a terminal with a prompt, and elsewhere with
`--allow-adopt`, the same shape `use` applies to billing and to banked resets.

```
$ codexctl login amir@sawmills.ai
codexctl: profile 'amir@sawmills.ai' does not record which account it holds,
          so this login cannot be matched to it. this login is 6df34c28….
          Replace it? Its saved credentials are overwritten. [y/N]
```

Answering `y` records the workspace, so the profile can answer for itself and
the question is not asked again. Declining changes nothing. Without a terminal
the answer defaults to no:

```
error: profile 'amir@sawmills.ai' does not record which account it holds, so
       replacing it needs approval and none was given. Re-run on a terminal to
       confirm, pass --allow-adopt, or choose another alias.
```

`--allow-adopt` settles only what the store could not work out. It has no
effect on a conflict the store _did_ work out: a stored workspace that
positively differs from the arriving one stays refused with or without it. It
also has to name its target — `codexctl save --allow-adopt` without an alias is
refused, because `save` would otherwise derive the alias from the token's email
claim and the flag would approve replacing a profile the operator never saw.

Two habits of a pre-release store change as a result.

**A claimless profile stops absorbing rotations that name a workspace.** Codex
refreshes tokens in place, and `codexctl` folds those back into the profile
that owns them. A profile whose stored token declares no workspace no longer
receives a rotation that declares one — that token may belong to another seat
of the same login. The profile keeps working; its stored copy goes stale, and
`status` shows the token ageing. `codexctl save` or a re-login brings it
forward and records the workspace, after which rotations are captured normally
again.

**A profile whose token no longer parses.** Its metadata may still record the
workspace, but the stored login cannot be read, so nothing proves the arriving
credential is its owner. This asks the same question. Answering `y` replaces it
in place — which is what `remove` followed by a fresh login would do anyway,
except that `remove` first destroys the metadata describing what was there. A
profile with nothing left at all — no readable token and no recorded account —
is asked about too, rather than quietly replaced: that verdict rests on the
token having failed to parse, and a future parser change must widen the
questions rather than the permissions.

## Shell completions

```bash
# zsh (source-based)
codexctl completions zsh > ~/.cache/zsh/completions/_codexctl

# bash
codexctl completions bash >> ~/.bashrc

# fish
codexctl completions fish > ~/.config/fish/completions/codexctl.fish
```

Completions dynamically list profile names for `use` and `remove`. zsh and fish also complete
`exec --account`; bash leaves it out on purpose, because its rules bind by command name and would
take over completion for the shell's own `exec`.

## How it works

Profiles are stored in `~/.codexctl/profiles/<alias>/` — each containing a copy of `auth.json` and `meta.json`. `codexctl login <alias>` runs `codex login --device-auth` with a unique isolated `CODEX_HOME` under `~/.codexctl/login-homes/<alias>/`, imports that auth file, removes the temporary login home, then switches to the saved profile. Switching copies the profile's `auth.json` into `~/.codex/auth.json`. `codexctl exec` copies it into `~/.codexctl/exec-homes/<alias>/auth.json` instead and passes that directory to the child as `CODEX_HOME`, so a pinned launch preserves the live auth file and active marker. It provisions its own exec home and can capture rotated credentials back to the saved profile.

The live auth file and active marker are separate atomic files. A switch installs auth first and
writes the marker last. If the process stops between those writes, the marker can remain on the
previous alias. Status reads then use saved profile auth instead of attributing the new live auth
to the old alias. Run `codexctl use <alias>` again to reconcile both files.

Rate limits are fetched from `chatgpt.com/backend-api/wham/usage` using the stored access tokens.
Usage requests send the account ID when available so that the usage response is scoped to the
intended account/workspace. Native Codex inference requests use the access token's
`chatgpt_account_id` workspace claim for scoping; the helper verifies that claim against the
selected connection and does not write a static `chatgpt-account-id` header. A static header would become stale when
`codexctl use` moves running sessions to another seat of the same login. Windows are matched by
their declared duration rather than by position, since plans that publish only a weekly limit
return it in the `primary_window` slot.

Live validation covered one login with personal and team seats: requests without the static header
returned `ok` for the personal workspace and that workspace's out-of-credits error for the team
workspace, matching requests with the corresponding header. This confirms that the token claim
scopes Codex inference to the selected workspace.

Banked resets use `wham/rate-limit-reset-credits` to list credits and
`wham/rate-limit-reset-credits/consume` to redeem one. Redemptions carry a client-generated
idempotency key, so retrying a timed-out request never spends a second credit.

Supports both Codex CLI auth formats:

- Nested: `{"auth_mode": "chatgpt", "tokens": {"access_token": "..."}}`
- Flat: `{"access_token": "..."}`

## License

Apache-2.0

## Experimental Central Account Server

The optional `central-prototype` feature builds `codexctl-central` for server-owned
credentials and registered local clients. See the [prototype guide](docs/central-prototype.md)
for setup, SSH tunnels, tests, and the remaining compatibility limits.

## Server-managed accounts

Connect your machines with company SSO and keep OpenAI refresh credentials on a private server.
Use `codexctl connect --server https://YOUR-SERVER`, then `codexctl use` and `codexctl codex`.
Resume a session from before migration with `codexctl codex resume <session-id>`.
On a connected machine, `codexctl login <new-alias>` adds an account to the server; the server
runs the OpenAI device login, so the refresh token never reaches your machine. Use `--local` for
a local profile instead.
See [server setup, migration, and staging deployment](docs/central-server.md).
