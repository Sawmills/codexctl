# Local Profiles

Use these steps on a machine without an account-server connection. Profiles keep
credentials on one machine. For shared accounts,
start with [server accounts](central-server.md).

## Save Accounts

Bootstrap each Codex account through `codexctl` so the Codex login runs in an isolated auth home:

```bash
codexctl login amir@example.com    # opens Codex device login, saves as profile

codexctl login amir+2@example.com
```

After an account is saved, switch with `codexctl use <alias>` instead of running `codex login`
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

## Switch Accounts

Direct:

```bash
codexctl use amir+5@sawmills.ai
```

Interactive fuzzy picker:

```bash
codexctl switch
```

## Codex App-Server Daemon

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
sessions; its own session relaunches with the flags it was started with. A pinned `exec` home keeps its daemon state separate from `~/.codex`.

## Pin a Local Command

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
The account server requires Unix process and shell support.
Windows tests require the same symbolic-link permissions as `exec`.
Windows daemon synchronization has only mock-daemon test coverage.
Compatibility with a real Windows Codex daemon remains unverified.

## Two Accounts on One Email

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
so this is stable across re-logins; including when the alias you type is not
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

## Adopt an Older Store

An account is identified by its workspace and its login together; neither
alone, because a workspace holds many people and one login holds seats in many
workspaces. Older profiles may record neither, so `codexctl`
reads them out of the stored token and remembers what it finds.

Two credentials that positively disagree are never the same account, and
`codexctl` refuses to overwrite one with the other. A profile that declares
_nothing_ is a different situation: the store cannot tell whether the account
arriving is the one it holds, but the operator who just logged in can. Those
cases ask rather than refuse; on a terminal with a prompt, and elsewhere with
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
also has to name its target; `codexctl save --allow-adopt` without an alias is
refused, because `save` would otherwise derive the alias from the token's email
claim and the flag would approve replacing a profile the operator never saw.

Two habits of a pre-release store change as a result.

**A claimless profile stops absorbing rotations that name a workspace.** Codex
refreshes tokens in place, and `codexctl` folds those back into the profile
that owns them. A profile whose stored token declares no workspace no longer
receives a rotation that declares one; that token may belong to another seat
of the same login. The profile keeps working; its stored copy goes stale, and
`status` shows the token ageing. `codexctl save` or a re-login brings it
forward and records the workspace, after which rotations are captured normally
again.

**A profile whose token no longer parses.** Its metadata may still record the
workspace, but the stored login cannot be read, so nothing proves the arriving
credential is its owner. This asks the same question. Answering `y` replaces it
in place; which is what `remove` followed by a fresh login would do anyway,
except that `remove` first destroys the metadata describing what was there. A
profile with nothing left at all; no readable token and no recorded account ;
is asked about too, rather than quietly replaced: that verdict rests on the
token having failed to parse, and a future parser change must widen the
questions rather than the permissions.

## Shell Completions

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

## Local Storage

Profiles are stored in `~/.codexctl/profiles/<alias>/`; each containing a copy of `auth.json` and `meta.json`. `codexctl login <alias>` runs `codex login --device-auth` with a unique isolated `CODEX_HOME` under `~/.codexctl/login-homes/<alias>/`, imports that auth file, removes the temporary login home, then switches to the saved profile. Switching copies the profile's `auth.json` into `~/.codex/auth.json`. `codexctl exec` copies it into `~/.codexctl/exec-homes/<alias>/auth.json` instead and passes that directory to the child as `CODEX_HOME`, so a pinned launch preserves the live auth file and active marker. It provisions its own exec home and can capture rotated credentials back to the saved profile.

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
