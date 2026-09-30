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

## Daily Use

```sh
codexctl status
codexctl list
codexctl use
codex
```

Automatic selection uses only accounts with verified included usage.
It prefers an account with headroom and the soonest long-window reset.
`CODEXCTL_SELECT=most-available` selects by headroom instead.
Explicit credit-billing selection still requires consent or `--allow-billing` on a non-interactive terminal.
The helper checks billing again before it supplies a token.
Organizational plans and accounts with credit evidence need a closed spend cap to qualify for automatic selection.
A missing or open cap for those accounts requires billing consent.
Subscription credits alone do not prove that further spending is disabled.
No remote command redeems a banked reset implicitly.
Finish existing TUI sessions and stop the daemon before switching accounts.
Existing sessions keep the account they selected at startup.
Local recovery wrappers remain separate from remote provider use.

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

Create an OIDC configuration file:

```json
{
  "issuer": "https://YOUR-COMPANY-OIDC-ISSUER",
  "client_id": "codexctl",
  "client_secret_file": "/keys/oidc-client-secret",
  "allowed_domains": ["sawmills.ai"]
}
```

Register the exact callback URL `https://codexctl.ue1.staging.plat.sm-svc.com/auth/callback` with your company identity provider.
Use a dedicated client secret in a private file with mode `0600` or stricter.
OIDC verification covers the signature, issuer, audience, expiry, nonce, and verified company email.
It keys users by issuer and subject, so an email change does not transfer account ownership.
The browser must approve each device after sign-in.
Enrollment expires after five minutes and can supply a credential only once.

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
Loopback HTTP remains available for local tests.

## Staging Deployment

The image definition is `deploy/Dockerfile`.
Use the staging overlay at `deploy/k8s/overlays/staging`.
Review the Argo application definition at `deploy/k8s/staging-application.yaml`.
That definition has no automatic sync.
Before registration, pin both workload images to the built image digest.

One StatefulSet replica owns an encrypted `ReadWriteOncePod` volume.
Do not add replicas, an autoscaler, or a second server with a copy of the credentials.
The file lock protects the mounted state from a second broker process.
Storage remains after scaling or deletion.
Confirm CSI support for `ReadWriteOncePod` before deployment.

Create these SSM SecureString values through the operator secret workflow:

- `/app/codexctl/vault-key`: base64 of the 32-byte encryption key.
- `/app/codexctl/oidc-client-secret`: the dedicated company OIDC client secret.
- `/app/codexctl/metrics-token`: a separate random bearer credential of at least 32 visible ASCII characters. The server trims surrounding whitespace.

External Secrets supplies the pod secret.
An init container copies the projected files into private real files for the broker.
Key rotation needs a separate re-encryption procedure. Do not rotate the key independently of the stored vaults.

The supplied SSO overlay points to the existing staging Dex issuer.
Dex documents a security concern with its existing SAML connector.
Resolve that concern or select a maintained company OIDC provider before live deployment.
See [the staging research](research/central-staging.md).

The staging VPC CNI currently has NetworkPolicy enforcement disabled.
The supplied policy does not yet restrict pod traffic in that cluster.
Confirm enforcement with the platform owner before relying on it for access control.

## Failure and Recovery

One owner process refreshes each account. Different accounts have separate request locks.
Server owners run in their private homes with the OpenAI provider and ChatGPT login mode fixed.
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
The pinned owner can supply separate permanent-failure evidence through `getAuthStatus`.
The broker requires an exportable cached ChatGPT token before the attempt and its suppression afterward.
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
A journal with a conflicting identity also fences new imports.
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
Owner failures during account listing have a separate bounded reason.
The staging overlay includes a PrometheusRule for the existing `kube-prometheus` selector.
`deploy/monitoring/servicemonitor.yaml` supplies the separate authenticated scrape.
The current staging ServiceMonitor schema has no authentication fields.
Update that schema before registering this scrape definition.
The unavailable rule also detects a missing scrape.
The metrics endpoint uses a separate credential from `/app/codexctl/metrics-token`.
This credential has no account API access.
Rules cover unavailable scrapes and recent credential or persistence failures.
Failure timestamps allow the first failure to alert even before a zero counter was scraped.
Alert routing and notification delivery are separate deployment checks.

## Verification Boundary

The integration suite uses an external Codex protocol fixture and an OIDC issuer with real RSA signatures and PKCE validation.
These tests run the real server and CLI processes.
It does not prove live company SSO, staging deployment, or seven-account migration.
The earlier prototype proved the native TUI helper with a dedicated live account on two machines.
Repeat that acceptance against the final staging image after SSO registration and deployment.

The OIDC RSA dependency has a narrow public-verification exception for RUSTSEC-2023-0071. See [the dependency analysis](research/central-staging.md#rsa-dependency-exception).
