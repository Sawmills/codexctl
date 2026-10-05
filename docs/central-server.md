# Server Accounts

Codexctl connects each machine to a private HTTPS account server.
Each company user owns a separate account catalog and a set of registered devices.
The server keeps the refresh credentials. Regular Codex receives access tokens through its existing provider helper.
Codex sends inference requests directly to OpenAI and runs tools on your machine.

## Browser Overview

Open the account server's root URL for installation commands and company SSO sign-in.
The read-only `/accounts` page shows only your server accounts and machines, with
usage windows, live reset countdowns, banked resets, and observation freshness.
It refreshes every 60 seconds through the existing usage cache. See the
[browser overview](web-dashboard.md) for authentication, schema, and cache details.

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

Use `codexctl status --json` or `codexctl list --json` for automation.
See the [versioned JSON schema](status-json.md) for fields and failure behavior.

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
Redeem a server-account reset with `codexctl resets --redeem <alias>` (add `--yes` for unattended use).
The list command never spends a reset or approves credit billing.
Both server and local redemption require an exhausted window and select the qualifying reset closest to expiry.
`codexctl use --allow-resets` can redeem when no account has included headroom.
Automatic selection excludes usage-based accounts and checks every local activation fence before spending.
After redemption it retries billing reads briefly to allow usage to settle, without approving credit billing.
If included usage remains unconfirmed, it reports that the reset was spent and activation is incomplete; wait and retry `codexctl use` without `--allow-resets`.
Explicit account selection never redeems a reset. `--allow-resets` and `--allow-billing` remain separate approvals.

The read-only `GET /v1/resets` endpoint requires a registered machine credential.
It returns only that company user's aliases, counts, and reset details, with `Cache-Control: no-store`.
The refresh owner supplies credentials for the server's OpenAI reads. The response contains no OpenAI credentials.
After OpenAI rejects an access token, the server records `reset_auth_rejected` and retries once through the refresh owner.
A successful retry does not trigger the operational failure alert.
Each failed account read increments `codexctl_central_failed_requests_total{reason="reset_read_failed"}` once.
A structured log identifies the `resets` stage. The existing `CodexctlCredentialOperationFailed` alert includes this reason.
The server rechecks machine authorization before delivery.
The company-user-scoped `POST /v1/resets/redeem` endpoint accepts an alias and a redemption request ID.
The server rechecks applicability, chooses the credit, and keeps OpenAI credentials with the refresh owner.
It saves the credit and provider idempotency key before sending, then saves the result.
Concurrent requests for one account are serialized. Repeating a completed request returns `already_redeemed`.
An uncertain result retains the original credit and key across retries and server restarts;
another operation cannot spend a second reset until that result is resolved.
The client also persists its request ID. After a timeout or server error, rerun the same redemption command.
If that machine is lost, another registered machine of the same company user can reconcile the pending operation.
The server reuses the original credit and provider key and records the result for both request IDs; it never starts a second spend during reconciliation.
Definitive non-retryable provider 4xx responses and unrecognized redemption codes close the operation with `reset_rejected`.
The failure receipt prevents a repeated ID from sending again, and the client clears its pending ID so a deliberate new command can try again.
Transport failures, HTTP 408/429, server errors, and unreadable responses retain the pending operation.
Do not delete the pending request files to bypass an unresolved outcome.
A retry of an already-sent operation may query the provider even after the exhausted window has cleared;
it uses the original idempotency key and cannot authorize another spend.
The server finishes persistence after a client disconnect and drains redemptions during graceful shutdown.
Provider and persistence failures increment the bounded `reset_redeem_failed` metric; terminal provider rejections use `reset_rejected`.
Both are included in the existing operational alert. Structured diagnostics retain the cause, redact stored credential strings, and limit the payload.
This endpoint requires deployment of the new account server; upgrading the CLI alone is insufficient.
Alert routing and notification delivery still require deployment checks.

Account listing never calls the refresh owner or changes its availability.
The server caches usage for 60 seconds and shares one fetch across concurrent polls
for the same account. On a cache miss or expiry, it uses the saved access token
for a read-only usage request outside the credential-owner lock, with a 15-second
timeout. It never refreshes credentials for listing. A failed fetch retains the
last observation and delays the next attempt for 60 seconds.

The account API also carries `primaryWindowSeconds`, `secondaryWindowSeconds`,
and `primaryResetsAt` beside the existing long-window `resetsAt`.
Durations come from the observed quota windows, in seconds, and are null when unknown.
Reset times on this API are Unix seconds. The CLI exposes both reset times as UTC
strings and the durations in [status JSON](status-json.md).
Both the account server and CLI need this update; an older server leaves the new
values unknown. Stale observations retain their durations and reset times with
the same freshness flags as their usage percentages.

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

Automatic selection uses only accounts with verified included usage.
Pro and Plus subscriptions (including `prolite` and `promax`) qualify while every
reported usage window has headroom, even when credits are available and the cap is open.
Both bare `codexctl use` and `codexctl use <alias>` allow this selection without `--allow-billing`.
Selection prefers the soonest long-window reset.
`CODEXCTL_SELECT=most-available` selects by headroom instead.
Within each billing class, automatic selection skips any candidate with a reported usage window at
or above 95% whenever another candidate in that class is below 95%; this applies to both local and
server-account selection.
Thus a server seat at 97% is skipped in favor of a 10% seat even when the 97% seat resets sooner.
An exhausted window (100% or more), invalid usage, or unknown entitlement requires billing consent.
Overage-limit evidence still requires a closed cap or consent, even with subscription headroom.
Usage-based accounts never qualify for automatic selection.
Explicit credit-billing selection requires consent or `--allow-billing` on a non-interactive terminal.
Organizational plans still need a closed spend cap, reported as `spendControlReached = true`.
No flag for billing consent approves a banked reset.

The provider uses `auth.refresh_interval_ms = 60000`.
The helper asks the account server for current billing evidence on every refresh.
After included usage ends, the helper returns no token unless the account has billing approval.
A failed billing read also refuses unapproved token delivery.
If credentials rotate during billing or routing checks, the broker repeats those checks once.
Further rotation refuses delivery until the operator retries with stable evidence.

After OpenAI reports the weekly limit, unapproved new requests stop at the next refresh, within one 60-second cache interval.
An in-flight response may finish, and usage reporting can lag, so a small credit spend remains possible.
This policy is not a hard spending cap or token expiry.
[Codex 0.159.0 caches helper tokens](https://github.com/openai/codex/blob/rust-v0.159.0/codex-rs/login/src/auth/external_bearer.rs#L33-L46)
until their age reaches the configured interval, then runs the helper before the next request.
Zero disables that age check; 60000 milliseconds limits normal cache reuse to one minute
without a billing read before every model request.
A helper failure supplies no replacement token.
The interval does not stop an in-flight response, revoke an issued token, or remove delays in OpenAI usage reporting.
Such work can continue beyond one minute; the interval is a bound on normal cached-token reuse, not total credit spend.
Run `codexctl use` after upgrading the client, then start new sessions so they load the new interval.
Server reset redemption requires an explicit redemption command or automatic selection with `--allow-resets`.
Switching between server accounts updates the provider and the active pointer.
Existing sessions refresh their helper token from that pointer within 60 seconds.
If the daemon is running, use `codexctl use <alias> --restart-daemon` to apply the switch.
After the provider rewrite and session repair, this restarts the daemon and resumes
the running and usage-limited sessions with the same continuation prompt and permissions as local switching:
no sandbox and no approval prompts (`approvalPolicy=never`, `sandbox=danger-full-access`).
Completed sessions receive no new turn. Failed resumes are reported by session ID.
Server-account resumes explicitly select `codexctl-central`, including when an open
or recent rollout still records `openai`. The daemon must confirm that provider
before codexctl starts a continuation; otherwise that session is reported as not resumed.
If activation fails before the restart, the error warns that the daemon still runs
the old account. Resolve the activation error, then rerun the switch with `--restart-daemon`.
Without `--restart-daemon`, server-account selection still refuses a running daemon;
finish its sessions and stop it manually before switching.
The flag does not start a daemon when none is running and does not approve credit billing.
Pending local logins and local recovery wrappers also block remote activation.
Parallel local commands remain supported.
An explicit saved local alias can restore local mode before migration, even when the server is unavailable.
Transferred aliases require the server. A known remote connection with the same alias blocks local selection.
Device login refuses a transferred alias even after disconnect.
An enrolled machine uses `codexctl login <alias>` to renew an existing server account.
The command prints an OpenAI device code and opens the OpenAI sign-in page.
Use `--no-browser` to open that page on another machine.
Approve the same OpenAI login and workspace as the selected alias.
The server stops that account's previous refresh owner before sign-in.
It retains the replacement credentials in a private server home and checks the account identity before replacement.
It then verifies the replacement owner before reporting success.
Your machine's OpenAI credentials and active account do not change.
Your other enrolled machines keep their registration and aliases.

If your terminal disconnects, run the same login command to resume.
Resume from the machine that started the login. Other machines cannot resume or cancel its pending login.
To stop a pending login, run `codexctl login <alias> --cancel`.
A canceled or failed login leaves that account unavailable until you retry.
Cancellation does not undo an authorization that OpenAI already issued.
Approving a different OpenAI account can invalidate that other account's previous grant.
The server retains a wrong-account grant without replacing the selected alias.
It stops a known owner of that wrong grant until the rightful user renews that account.
Other accounts remain available.
A migration whose login identity conflicts with retained credentials returns HTTP 409 with `alias_identity_conflict`.
The account stays fenced. This refusal does not record a recovery failure or trigger the credential-operation alert.

Without server registration, `codexctl login` keeps its local behavior.
Login first fetches the current account catalog, so accounts created on another machine
can renew without a prior `list` or `status` command, even while a server provider is active.
If discovery fails, the machine uses its last successful catalog and retained migration
or connection records to distinguish known server aliases from local aliases.
Known server aliases require the account server; an outage never starts local login for them.
Other aliases can still start local login when the server is unavailable.
Local login keeps the existing migration and active-provider checks.
`whoami` reports the active account. Retiring live credentials also clears the local active marker.
After disconnect, it reports a local profile only when local credentials remain active.
The native provider currently supports the global ChatGPT backend with no regional routing constraint.
Before each token delivery, the server checks workspace routing.
The access token's `chatgpt_account_id` claim scopes native requests to its workspace. The helper
verifies that claim against the selected connection before printing a token, and activation does
not write a static `ChatGPT-Account-ID` header: a static value would become stale when the active
pointer moves running sessions between seats held by one login.
Regional routes, routing overrides, and missing routing evidence refuse import verification or activation.
Read-only brokers cannot verify native routing and cannot supply the native provider.
Live validation covered one login with personal and team seats: a request without the static header
returned `ok` for the personal workspace and that workspace's out-of-credits response for the team
workspace, matching requests with the corresponding header. The token claim therefore scopes Codex
inference to the selected workspace.
Unpinned server sessions use the active account pointer when they refresh their helper token.
After `codexctl use <alias>`, every unpinned running session on this machine moves to the selected
account within 60 seconds. If that account can bill credits, all of those sessions can bill
credits after their included usage ends; the switch still requires billing approval.
Use `codexctl codex resume <session-id>` to resume a session from before migration.
The launcher passes `-c 'model_provider="codexctl-central"'` to Codex.
Codex 0.160.0 otherwise restores the session's saved provider, even when the base configuration selects a server account.
An old `openai` session can then fail compaction with HTTP 401 because migration retired its local credentials.
For a direct launch, use `codex resume <session-id> -c 'model_provider="codexctl-central"'`.
The account-server provider uses local compaction through its authenticated Responses connection.
The launcher preserves the current directory, arguments, and child exit status.
Codex and its child processes inherit a mode lease.
Entry from local mode and migration require an exclusive lease, including when a local login child outlives its launcher.
Switching an already active server provider shares the session leases and serializes configuration writes.
If disconnect or local selection restores the provider during that switch, selection refuses and asks for a retry.
It refuses inherited or pinned Codex homes.
Server-account launches use the provider token helper without local account failover or banked resets.
`codexctl exec --account <alias> -- <command>` supports saved local profiles only.
To pin one Codex session to a server account without switching the host account:

```sh
codexctl codex --account personal "a new prompt"
codexctl codex --account personal resume <session-id>
codexctl codex --account team --allow-billing resume <session-id>
```

The launcher gives the child a private connection and explicit provider/token-helper
arguments. It preserves the working directory and resumed session; the host's
configuration, active pointer, and auth file stay unchanged. Later `codexctl use`
switches do not move this session. No static `ChatGPT-Account-ID` header is sent:
the token workspace claim must match the selected account on every helper refresh.
Billing approval applies only to this launch. SIGHUP and SIGTERM remove its private
connection before forwarding the signal to Codex; an unresponsive child is killed
and reaped after one second. Helpers refuse connections whose launcher no longer
holds its owner lock, including after SIGKILL. The next launch and `disconnect`
sweep orphaned `lanes/launch-*` directories under an exclusive lane-directory lock,
while preserving directories owned by live launchers. Accounts that may bill credits require
confirmation on a terminal or `--allow-billing` for unattended use. Exhausted accounts
require `--allow-billing` to launch on ChatGPT credits. Helper refreshes keep serving
that account while its approved plan and billing class match. This launch never redeems a reset and rejects `--allow-resets`.
Inherited `CODEX_HOME`/pinned launches and provider/profile argument overrides refuse;
a selected config profile that overrides `model_provider` must be removed first.
Exit status is the child's status (including `128 + signal` on Unix), 1 for launch
or policy failures, and 2 for invalid CLI options. Herdr pane management stays outside
codexctl: stop the intended lane, then run the pinned launch in that lane.

### Read per-account response rates

```sh
codexctl rate
codexctl rate --json --minutes 10
```

`rate` opens `~/.codex/logs_2.sqlite` read-only, including committed WAL entries, and
uses the same account usage lookup as `status --json` (which may update status caches).
It never switches accounts or redeems resets. The default window is 10 minutes;
`--minutes` accepts 1 through 525600. Every server account has a row, including
unavailable accounts and accounts with no matching traffic. Their response counts,
429 ratio, and log process count are zero. A missing log database produces these
zero-traffic rows without creating a database; unreadable or malformed databases
still fail with exit 1. Only successful HTTP responses and 429s from
`/codex/responses` count; other statuses and endpoints are excluded.

The `x-codex-primary-reset-at` response header is matched to the account's declared
short window (including 5-hour windows) or weekly reset, allowing 120 seconds of drift.
When present, `x-codex-primary-window-minutes` must also match the status window's
declared duration; malformed or mismatched durations remain unattributed. The legacy
secondary weekly slot may omit its duration and still match a 10080-minute header,
but never a short-window header. Without that header, a reset can match either known
window. A short window without a declared duration is never inferred. Matching windows
from different status rows (including local/server rows sharing an alias), unknown
reset times, and missing evidence stay `.unattributed` (a reserved label that cannot
be an account alias). Two matching windows from the same row are not ambiguous.
Headerless replies inherit the last observed
assignment for the same process within the requested window; an unrecognized header
clears that assignment. An account switch without a logged header cannot be detected.
`processes` counts distinct process identifiers observed in the logs, not live OS
processes. `pids` contains the parseable PID segment in `pid:<pid>:<uuid>` identifiers, deduplicated and sorted;
reused PIDs can correspond to multiple process identifiers.

JSON contains `host`, `window_minutes`, `generated_at`, and `accounts`, whose rows
contain `account`, `weekly_used_percent`, `responses_ok`, `responses_429`, `rate_429`,
`processes`, `pids`, and `owned_pids`. `rate_429` is 429s divided by successes plus
429s (zero without responses); unknown weekly usage is null.

When live process inspection is unavailable, JSON also contains a `warnings` array;
log counts remain available. Linux uses procfs for live ownership; other Unix systems
report no ownership when `ps` fails. In text mode, warnings go to stderr
and the table includes an `Owned PIDs` column. An empty warnings array is omitted.

`owned_pids` is an array such as `[{"pid":123,"source":"launch"}]`, sorted by PID.
On Linux and macOS it reports live processes independently of the response window:

- `launch`: a live `codexctl codex --account` launcher owns the direct child and
  native Codex descendants. If `ps` is unavailable, Linux procfs supplies the same
  Codex child rows. Its private `lanes/launch-*/owner.json` records the
  launcher PID and process incarnation; both that identity and the held owner lock
  must still be valid. Older launches without this metadata can use log evidence.
- `host-default`: an unpinned Codex process in this home uses the host's active
  server-account pointer. Isolated homes and inherited pinned aliases are excluded.
- `log`: when neither source identifies the account, the latest reset-header
  evidence from the live process's lifetime can identify it, even outside the
  requested response window. Ambiguous or invalid newer headers clear this evidence.

Sources have the precedence shown above. Ownership is a process snapshot and can
change after the command returns. Exited processes disappear from `owned_pids`;
the historical `pids` and response counts stay unchanged. A host-default assignment
states the configured default, not proof that a request used it. Local-only Windows
builds retain log counts and return empty `owned_pids`.

The backend exposes no token-per-minute counts or limit, so neither field is
estimated or included. Response counts are log evidence, not a throughput quota.

Local-account launches retain the existing recovery behavior.
Run `codexctl use` after an upgrade to refresh the provider helper path and see the launch command.

## Manage Devices and Users

```sh
codexctl devices
codexctl devices --revoke 'device-id'
codexctl disconnect
codexctl disconnect --forget
```

Revocation blocks future server requests.
An access token already supplied to Codex remains usable until OpenAI rejects it or it expires.
`disconnect` restores the previous provider and keeps registration.
`--forget` also removes the local registration and device credential.
If credential removal fails, the registration stays available for a cleanup retry.
If sign-in finishes but local installation fails, run `codexctl disconnect --forget` before enrolling again.
It does not revoke the server record.
Provider restoration and registration removal hold one client lock through completion.

Server operators can list users and disable their devices through user access control:

```sh
codexctl-central users --state /data/state
codexctl-central users --state /data/state --user USER_ID --disable
codexctl-central users --state /data/state --user USER_ID --enable
```

A disabled user cannot enroll or use an existing device.
The API does not provide administrators with another user's OpenAI tokens.
Host administrators with the encryption key and storage can decrypt credentials.

## Run the Server

Build the standard binaries with `cargo build --release --locked`.
The central feature is enabled by default.
`--no-default-features` builds the local account manager without central support.

Initialize an empty state directory:

```sh
codexctl-central setup --state /data/state --key-file /keys/vault-key
```

The key contains 32 raw random bytes.
An existing private key file can come from the secret store.
Setup creates a key when the destination does not exist.
It saves the device registry before the user registry, which marks completed setup.
An interrupted setup can reuse the key and complete both registries.
Keep the key separate from encrypted storage and backup both.

The active staging overlay remains on Clerk. For the planned Google Workspace
cutover, prepare an OIDC configuration file:

```json
{
  "issuer": "https://accounts.google.com",
  "client_id": "REPLACE_WITH_GOOGLE_WEB_CLIENT_ID.apps.googleusercontent.com",
  "client_secret_file": "/keys/oidc-client-secret",
  "allowed_domains": ["sawmills.ai"],
  "allowed_hosted_domains": ["sawmills.ai"]
}
```

The planned company SSO setup uses Google Workspace directly. In the Google Cloud
project owned by `sawmills.ai`:

1. Open **Google Auth platform** (or **APIs & Services → OAuth consent screen**),
   configure the application name, support email and developer contact, and set
   **Audience / User type** to **Internal**. This requires a project within the
   Workspace organization. Do not select External as a workaround.
2. Under **Clients → Create client** (or **Credentials → Create credentials →
   OAuth client ID**), choose **Web application**, named `codexctl-staging`.
3. Add exactly
   `https://codexctl.ue1.staging.plat.sm-svc.com/auth/callback`
   as an **Authorized redirect URI**. No JavaScript origin is required for this
   server-side authorization-code flow. It requests only `openid email` and uses
   PKCE. Do not request offline access or an OpenAI scope.
4. Copy the client ID into `deploy/k8s/cutovers/google-workspace/sso.yaml`. Store the client
   secret as a **plaintext secret value** in AWS Secrets Manager under
   `/app/codexctl/oidc-client-secret`; do not store a JSON object around the value.
   Never commit the secret. The planned cutover ExternalSecret entry reads this name through
   `ClusterSecretStore/aws-secrets-manager` into `codexctl-secrets`, and the init
   container copies it to `/keys/oidc-client-secret` as a private real file.
   The platform administrator must provision/verify that ClusterSecretStore with
   provider service `SecretsManager` and permission to read this secret before
   cutover. The existing `aws-parameter-store` store continues serving the vault
   key and metrics token; it cannot read the new Secrets Manager value.
5. Schedule the switch and prepare the explicit Clerk links below. Confirm the
   Google client, matching secret, secret store and reviewed image are ready
   before manually registering or syncing the Application for cutover. Restart
   the account server after that planned sync.
   The files under `deploy/k8s/cutovers/google-workspace/` are unreferenced
   preparation templates, with a placeholder client ID and Amir's company-user ID/email
   allowlist. Fill the client ID and verify that allowlist, then promote the manifests into
   `deploy/k8s/overlays/staging/` in a separate reviewed cutover change. Until then,
   staging keeps its Clerk client and SSM secret source. See the
   [cutover checklist](../deploy/k8s/cutovers/google-workspace/README.md).

OIDC verification covers the signature, issuer, audience, expiry, nonce and
`email_verified == true`. The email must have an allowed company domain. For
Google, the signed ID token must also carry `hd` matching an
`allowed_hosted_domains` entry (case-insensitive DNS comparison). This list defaults
to `allowed_domains` for `https://accounts.google.com`; an empty list is rejected.
Other OIDC issuers retain email-domain policy unless hosted domains are explicitly
configured. A consumer Google account can use a company-looking email address, so
email alone is insufficient. The authorization request sends `hd=sawmills.ai` as
an account-chooser hint for a single hosted domain (`hd=*` for several); the token
claim remains mandatory regardless of the hint.

Company users are normally keyed by issuer and subject, so an email change does
not transfer account ownership. The browser must approve each machine after
sign-in. Enrollment expires after five minutes and supplies a credential only once.

### One-time Clerk identity cutover

Changing the issuer changes its subject namespace. An administrator with account
server configuration access may explicitly authorize **individual existing Clerk
company users** for one link on their first verified Google sign-in. There is no
browser or machine API for authorizing links. Back up the state and key first,
list company users with `codexctl-central users --state /data/state`, and confirm
the stored company-user ID and verified primary company email. The server stores
only the derived company-user ID, not the raw Clerk subject. Copy that ID from
`codexctl-central users`; no raw subject lookup is required. An administrator must
confirm that the selected record is the intended existing company user.

Temporarily add this object to the Google SSO configuration, using the exact
stored company-user ID and verified email values. The prepared staging allowlist
authorizes only Amir; it excludes the assistbot and CLI integration company users:

```json
"clerk_migration": {
  "users": [
    {
      "user_id": "6334462fa3dac53bdd9836507666cdde920a73768c3b52b632297d08130e1fd4",
      "email": "amir@sawmills.ai"
    }
  ]
}
```

Configuration write access is administrative authority: restrict it with the same
filesystem permissions and deployment RBAC as the server configuration. Do not
populate the allowlist from an untrusted email list. The account server requires
the exact approved company-user ID and agreement between the approved email,
the existing company user's email, and the verified arriving Google email. It
refuses ambiguous emails, disabled company users, a missing source, and any second
Google identity for a consumed source.
An existing company email without explicit authorization is refused rather than
creating a competing company user. New Workspace members with unused emails can
still enroll normally.

The link is recorded atomically under the user-registry lock. It keeps the
company-user ID, server accounts, encrypted vault paths and machine credentials
unchanged, and emits `SSO_IDENTITY_LINKED` with bounded identity digests (no email
or tokens). Subsequent sign-ins use the recorded identity without linking again.
Remove `clerk_migration` and restart after the approved cutover; recorded links
keep working and further links are disabled. The old Clerk identity cannot sign
in to a linked company user if the issuer configuration is switched back. Do not
roll back to a binary predating identity links against migrated state; use the
planned state-backup recovery procedure instead. See
[ADR 0003](adr/0003-explicit-company-identity-cutover.md).

Run behind the private HTTPS load balancer:

```sh
codexctl-central serve \
  --state /data/state --key-file /keys/vault-key \
  --listen 0.0.0.0:8787 \
  --public-url https://codexctl.ue1.staging.plat.sm-svc.com \
  --sso-config /configuration/sso.json
```

The load balancer terminates TLS and forwards HTTP to the private service.
Do not expose the backend listener outside that network.
Clients reject cleartext remote origins and redirects.
Saved registrations also pass origin validation before migration or device revocation.
Isolated local tests can set `CODEXCTL_ALLOW_INSECURE_LOOPBACK=1` to permit loopback HTTP.
Use only synthetic credentials or a dedicated test login with that option.

## Staging Deployment

The image definition is `deploy/Dockerfile`. Its allowlisted build context contains
only the Rust source, locked dependencies, and Dockerfile.
The `Central server image` workflow publishes reviewed `main` commits to private ECR.
Its dedicated AWS role can publish only the codexctl image. It cannot read runtime
secrets or access the cluster. Forks and non-main branches cannot assume that role.
The workflow publishes both Linux architectures and records the combined digest.
Set the repository variable `CENTRAL_IMAGE_PUBLISH_ROLE` to the dedicated role ARN.
Before registration, pin both workload images to that digest in the staging overlay.

The staging application lives in `Sawmills/argocd-deploy` at
`plat/ue1-staging/argocd/codexctl-application.yaml`.
The checked-in [Application](../deploy/k8s/staging-application.yaml) tracks `main`
and `deploy/k8s/overlays/staging`; its sync policy sets `CreateNamespace=true` but
has no automated sync, prune, or self-heal. Review and manually register the
Application if it is not installed; sync is also an explicit operator action.
Verify the installed Application's policy and the Google client/secret readiness
before the planned cutover sync.
The internal ALB terminates HTTPS. The broker uses a ClusterIP service.

One StatefulSet replica owns an encrypted `ReadWriteOncePod` volume.
Do not add replicas, an autoscaler, or a second server with a copy of the credentials.
The file lock protects the mounted state from a second broker process.
Storage remains after scaling or deletion.
Confirm CSI support for `ReadWriteOncePod` before deployment.

Create these SSM SecureString values through the operator secret workflow:

- `/app/codexctl/vault-key`: base64 of the 32-byte encryption key.
- `/app/codexctl/oidc-client-secret`: the current Clerk client secret.
- `/app/codexctl/metrics-token`: a separate random bearer credential of at least 32 visible ASCII characters. The server trims surrounding whitespace.

For the later Google cutover, prepare a separate AWS Secrets Manager value at
`/app/codexctl/oidc-client-secret`, as described above. The active ExternalSecret
continues reading the Clerk secret from SSM until that reviewed cutover.
External Secrets supplies the pod secret.
An init container copies the projected files into private real files for the broker.
Key rotation needs a separate re-encryption procedure. Do not rotate the key independently of the stored vaults.

The active SSO overlay uses `https://clerk.sawmills.ai`. The unreferenced Google
cutover files target `https://accounts.google.com` and the `sawmills.ai` Workspace;
they require the administrator-created Internal web client and verified migration
allowlist described above. The [Clerk OIDC research](research/central-staging-oidc.md)
describes the current provider, not the planned Google setup. Google documents
[Workspace `hd` verification](https://developers.google.com/identity/openid-connect/openid-connect#obtainuserinfo)
and [web client creation](https://developers.google.com/identity/protocols/oauth2/web-server#creatingcred).

The staging VPC CNI currently has NetworkPolicy enforcement disabled.
The supplied policy does not yet restrict pod traffic in that cluster.
Confirm enforcement with the platform owner before relying on it for access control.

## Failure and Recovery

One owner process refreshes each account. Different accounts have separate request locks.
Server owners run in their private homes with the OpenAI provider and ChatGPT login mode fixed.
Background recovery is controlled by `CODEXCTL_CENTRAL_BACKGROUND_RECOVERY`. It is
disabled by default. Explicit import remains available to repair an unavailable
owner through the normal identity and verification checks. Enable background
recovery only for a reviewed trial by setting the value to `1`, `true`, `yes`, or
`on`; the setting takes effect when the server starts.
Concurrent requests for the same old revision reuse the refreshed token.
Revisions cover the full credential state, including refresh-only rotations.
Client disconnection does not cancel a refresh or an import that already started.
A graceful shutdown drains requests, stops account owners in parallel, and saves credentials after owner exit.
A stable private runtime journal retains the latest Codex login file across restarts.
Startup checks the previous process identity, account identity, and token ordering before reconciliation.
Recovery keeps the newer journal or vault credentials and refuses unordered differences.
A failed reconciliation also blocks overlapping imports.
An interrupted import keeps its credentials reserved until the original alias resolves verification.
A generic `account/read` error does not prove credential rejection.
The two pinned routing-policy errors for a missing backend origin or an invalid routing override produce a recoverable routing refusal.
Other protocol errors still require owner diagnosis.
The pinned owner can supply separate permanent-failure evidence through `getAuthStatus`.
The cached-login check can proactively refresh credentials, so the broker reconciles its journal on every outcome.
It compares an exported token with the validated post-call journal.
For the pinned file-backed owner, a completed ChatGPT status with a suppressed token also identifies an initial permanent refresh failure.
Missing, unsupported, or ambiguous evidence keeps the candidate reserved.
A confirmed rejection with unchanged credentials allows another candidate.
The original alias can supply fresh same-seat credentials after its rejected owner stops.
A repair writes the replacement journal before the vault and keeps that journal across an interrupted repair.
After a definite executable start failure, fix the executable and retry migration.
Unknown starts still require process diagnosis.
Each replacement invalidates the old process record before it starts.
A replacement keeps the old ownership reservation until the new owner is prepared.
An unreadable account vault blocks new imports until an operator restores ownership evidence and restarts the broker.
Unreadable ownership reports a recovery failure instead of a missing account or an empty catalog.
Healthy existing accounts remain usable when unresolved previous processes are proven stopped.
Startup inventories every account before it launches replacements.
A stopped candidate with a readable conflicting journal remains quarantined.
A completed request and confirmed process exit permit quarantine even when the process exits nonzero.
Both the vault identity and journal identity block overlapping imports; unrelated users can continue onboarding.
Startup blocks a replacement for any existing account named by that conflicting journal.
This overlap check includes legacy journals that omit a known user ID.
A journal under an unreadable vault also blocks matching replacements.
If a runtime exists and neither file identifies its owner, startup blocks all replacements.
An inaccessible account directory also keeps that fence in place.
A live or unidentifiable process under unreadable ownership blocks all replacement launches.
An unresolved-ownership metric keeps the operational alert active during that fence.
On Linux, a parent-death signal kills the owner when the broker dies.
Kubernetes storage attachment and pod identity provide additional ownership guards.
Do not force-delete a pod while its node can still run the previous owner.

A lost refresh response before Codex saves it can still require a new login.
An identity conflict, a corrupt journal, or a surviving owner blocks recovery of that account.
Other valid accounts remain available.
Failed imports keep their owner process until pending work settles.
Shutdown drains detached imports before it stops those owners.
The server preserves that evidence. Do not replace it with an older backup automatically.
A server outage prevents token acquisition and refresh. Existing access tokens can continue to work for a bounded period.
This first deployment has one server and does not promise uninterrupted availability.

`/health` checks process availability. `/ready` checks readable user and device registries.
Authenticated `/metrics` records each rejected API attempt once, including JSON and body-limit rejections.
Usage fetch failures during listing have separate bounded reasons:
`catalog_usage_failed` and `catalog_usage_timeout`. The server counts each failed
fetch once, even when several polls share it. These reasons use the existing
account-operation alert. A usage alert does not mean token delivery has failed.
The staging overlay includes a PrometheusRule for the existing `kube-prometheus` selector.
The staging `ScrapeConfig` supplies an authenticated scrape through the existing
Prometheus operator. Its Secret reference selects only the metrics credential.
The ServiceMonitor schema lacks authentication fields, so this deployment uses
that supported ScrapeConfig path without changing the shared monitoring hold.
The endpoint declares Prometheus text format version 0.0.4.
Availability alerts use failed or absent scrapes. Credential alerts use recent
failure timestamps and the unresolved-ownership gauge.
The metrics credential at `/app/codexctl/metrics-token` has no account API access.
Failure timestamps allow the first failure to alert even before a zero counter was scraped.
Alert routing and notification delivery are separate deployment checks.

## Verification Boundary

The integration suite uses an external Codex protocol fixture and an OIDC issuer with real RSA signatures and PKCE validation.
These tests run the real server and CLI processes.
It does not prove live company SSO, staging deployment, or seven-account migration.
The earlier prototype proved the native TUI helper with a dedicated live account on two machines.
Repeat that acceptance against the final staging image after SSO registration and deployment.

The OIDC RSA dependency has a narrow public-verification exception for RUSTSEC-2023-0071. See [the dependency analysis](research/central-staging.md#rsa-dependency-exception).

## Repair Saved Session Providers

For an active server account, `codexctl use` repairs old saved session providers.
Plain `codex resume <session-id>` then uses the account server for those sessions.
Migration also runs the repair if a server account is already active.
Otherwise, migration defers the repair until `codexctl use` activates one.
The repair requires `lsof` on the machine. macOS includes it; install the `lsof` package on Linux.
A failed OS inspection stops the repair and reports any completed changes.
An empty `lsof` result with exit status 1 means no open files.
On Linux, inspection exempts confirmed kernel tracefs mounts at `/sys/kernel/debug/tracing` and `/sys/kernel/tracing`;
these cannot contain session rollouts, and their restricted permissions can otherwise prevent inspection.
Other inspection diagnostics, error exits, and ambiguous exit-1 output still stop repair.
If repair fails after account activation or migration, the error names that completed step.
Resolve the reported cause before you retry the repair.

Preview the files without changing rollouts or backups:

```sh
codexctl session-provider dry-run
```

Close sessions that the preview reports as `skipped-open`.
Wait until files reported as `skipped-recent` have no modifications for at least 60 minutes.
Run the repair after you inspect the list:

```sh
codexctl session-provider rewrite
```

The repair searches only `sessions` and `archived_sessions` under the active `CODEX_HOME`.
When `CODEX_HOME` is unset, it uses `~/.codex`.
The active server account must belong to that home.
It does not follow symbolic links or replace files with hard links.
Preview, rewrite, and restore skip any rollout modified within the last 60 minutes, including future timestamps.
The repair checks modification time again after the copy and before replacement.
This protects idle sessions that close their files between appends.
Only the `payload.model_provider` value in the first `session_meta` line changes from `openai` to `codexctl-central`.
Every other byte stays identical, including whitespace and line endings.
Other providers, missing provider fields, and valid legacy headers without a record type stay unchanged.
Malformed metadata stops the repair; metadata lines above 8 MiB also stop it.
The rest of each rollout streams through a temporary file in the same directory.
The replacement preserves permissions and modification time.
The repair syncs the replacement and its directory.

Private backups retain the original metadata line under
`CODEX_HOME/.codexctl-session-provider-backups/`, with the same relative rollout path.
A backup reaches disk before its rollout changes.
Repeated repair runs preserve the first backup.
The command prints the backup directory and the exact restore command.
The summary reports rewritten, unchanged, open, recent, linked, and missing-backup files, plus errors.
A failure preserves prior completed changes and backups for a retry.
If a process interrupts a copy, the original rollout stays intact and a temporary file can remain beside it.

To restore the original metadata while the server account is still active, run:

```sh
codexctl session-provider restore
```

Before local selection or `disconnect`, restore all repaired rollouts.
Both commands refuse to remove the server provider while a repaired rollout still needs it.
If restore skips open or recent files, close those sessions and wait for the 60-minute window before you retry.
This guard also finds repaired rollouts that moved to the archive.

Restore also skips open and recent files and preserves the rollout body, permissions, and modification time.
It refuses a file whose metadata no longer matches its backup or the repaired version of that backup.
After Codex archives or unarchives a rollout, restore matches its filename and exact metadata to the original backup.
Multiple matches or conflicting metadata stop restore.
Files with no backup appear as `skipped-no-backup`.
Keep the backups until you no longer need restore.
After restore, old sessions need the explicit provider override from `codexctl codex resume` again.

The OS inventory and the final per-file inspection include open files from other processes.
Each replacement repeats the OS inspection.
Large stores take longer on the first run; later runs need only the initial inventory and metadata reads.
Close Codex sessions before repair or restore and do not start sessions during the operation.
These OS inspections cannot prevent a process from opening a file after the final inspection.
