# Server Accounts

Codexctl connects each machine to a private HTTPS account server.
Each company user owns a separate account catalog and a set of registered devices.
The server keeps the refresh credentials. Regular Codex receives access tokens through its existing provider helper.
Codex sends inference requests directly to OpenAI and runs tools on your machine.

## Connect a Machine

Run this command on each machine:

```sh
codexctl connect --server https://codexctl.ue1.staging.plat.sm-svc.com --name "Amir MacBook"
```

Sign in with company SSO in your browser.
Compare the browser code with the terminal code.
Approve the machine in your browser.
For a machine without a browser, add `--no-browser` and open the printed link on another machine.
Automatic browser launch supports macOS and Linux. On other platforms, use `--no-browser`.

The client stores the server address, your immutable SSO identity, and a private device credential under `~/.codexctl/central`.
You do not need a token file or an SSH tunnel.
Your machine needs access to the private staging network.

## Transfer Existing Accounts

Before migration, stop every Codex session and credential owner for these accounts on all machines.
Stop the local daemon with `codex app-server daemon stop`.
Run the migration on the machine with your saved accounts:

```sh
codexctl migrate --all --exclusive-owner
```

The flag confirms that you stopped every previous refresh owner.
Migration also refuses a local Codex process or daemon.
It searches all profile, exec, and login homes by account identity, including profiles without metadata and recovery copies under another alias.
It captures newer credentials from known local homes before transfer.
A copy without enough workspace or namespaced login evidence blocks migration before any upload.
An inaccessible credential path also blocks migration and remote activation.
Reconcile that copy before retrying.
It keeps your aliases and labels.
It records each confirmed account and fences local credential refresh before upload.
A failed retry preserves an existing confirmation for the same server, user, alias, and account.
API-key-only credentials stay unchanged.
Migration preserves retired credentials in private backup files.
It removes matching profile, login session, and exec credentials from normal `auth.json` paths.
The client syncs each affected directory before upload.
A private `.central-source.json` file retains each input for safe retries.
It does not overwrite a server account on retry.

If migration fails, keep the old owners stopped and run the same command again.
A profile with uncertain completion stays fenced.
You can still list accounts and use another account that completed migration.
Discovery does not install native connections for every alias.
Do not remove its transfer marker or restore its old refresh token.
An existing account under another user causes a conflict.
Different logins under an existing alias also cause a conflict.

Other machines with old local copies need the same explicit handoff before their matching aliases can become remote accounts.
A successful retry reconciles the existing server account without replacing its current credentials.
If registration disappears during migration, the server transfer receipt stays confirmed, but the client refuses connection installation.
Enroll again before retrying.
A new machine with no local copies needs only enrollment.
Before activation, the client checks every known local credential home by account identity.
A copy under another alias or an uncertain identity requires an explicit handoff.

### Migration window on a busy machine

These steps come from a migration on 2026-10-02 with 28 Codex panes on one Mac.

1. Record each pane's session ID from its statusline or the rollout file that its Codex process keeps open.
2. Run `codexctl devices` on each machine and confirm that all machines show the same company user. Sign in to the browser with the correct company account.
3. Pause supervisors that automatically restart lanes so they cannot restart Codex during the migration window.
4. Stop Codex with a normal exit (Ctrl+C twice) or `codex app-server daemon stop`.
   Avoid SIGTERM: it leaves threads locked and can leave the terminal in an extended keyboard mode. Run `printf '\033[<u'` to restore the terminal.
5. Quit the Codex desktop app and any plugin app-servers. `codexctl migrate` refuses while any Codex process runs.
6. After migration, run `codexctl use <alias>`. Explicit credit-billing selection still requires consent or `--allow-billing` on a non-interactive terminal.
7. Resume each session with `codexctl codex resume <sid>`.
   Plain `codex resume` restores the saved `openai` provider and gets HTTP 401 at compaction. If a shell alias adds flags, do not repeat them.
8. When resume shows "This conversation is open in another app", press `r` once; the daemon releases the thread.
   Do not use the TUI `f fork` for this: a TUI fork keeps the `openai` provider.

## Daily Use

```sh
codexctl status
codexctl list
codexctl whoami
codexctl resets
codexctl use
codexctl codex
```

On a connected machine, `status` and `list` also show profiles that have not
started migration, marked `local`, with live usage. A failed usage request keeps
the profile visible with an error. Empty columns are hidden. When the server has
no accounts, a migration hint appears below the table. Profiles with a migration
marker remain excluded, including migrations whose completion is unknown.

`codexctl resets` lists banked counts, redeemable counts, and expiry dates for server accounts.
It reads those values through the account server. Migration does not require local `auth.json` files for this command.
Unmigrated profiles retain their local reset lookup.
A failed account read shows an error and an incomplete total. The command exits with failure in both server and local modes.
An account server without the reset endpoint rejects this command until the operator deploys the new server.
Server-account reset redemption remains unsupported. The list command never spends a reset or approves credit billing.
Local redemption still requires an exhausted window and selects the qualifying reset closest to expiry.
Explicit account selection never redeems a reset. `--allow-resets` and `--allow-billing` remain separate approvals.

The read-only `GET /v1/resets` endpoint requires a registered machine credential.
It returns only that company user's aliases, counts, and reset details, with `Cache-Control: no-store`.
The refresh owner supplies credentials for the server's OpenAI reads. The response contains no OpenAI credentials.
After OpenAI rejects an access token, the server records `reset_auth_rejected` and retries once through the refresh owner.
A successful retry does not trigger the operational failure alert.
Each failed account read increments `codexctl_central_failed_requests_total{reason="reset_read_failed"}` once.
A structured log identifies the `resets` stage. The existing `CodexctlCredentialOperationFailed` alert includes this reason.
The server rechecks machine authorization before delivery.
Alert routing and notification delivery still require deployment checks.

Account listing never calls the refresh owner or changes its availability.
The server caches usage for 60 seconds and shares one fetch across concurrent polls
for the same account. On a cache miss or expiry, it uses the saved access token
for a read-only usage request outside the credential-owner lock, with a 15-second
timeout. It never refreshes credentials for listing. A failed fetch retains the
last observation and delays the next attempt for 60 seconds.

The account API returns `usageAgeSeconds`, `usageStale`, and `usageError`.
Age is null until an observation succeeds. Status and list show stale or missing
usage in the Error column. Stale usage cannot authorize automatic account selection.
An expired access token can leave usage stale until a normal token request or login
renewal updates the credentials. An expired token reports `access_expired` without
an upstream request or a failure alert. Token delivery still checks billing and
routing. Listing retains routing refusals observed by token delivery until a
successful routing check clears them.
A quota monitor can poll every 60 seconds without overlapping requests and must
treat stale usage as unknown, not as available quota.
