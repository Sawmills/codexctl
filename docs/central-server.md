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
codexctl whoami
codexctl use
codex
```

Automatic selection uses only accounts with verified included usage.
It prefers an account with headroom and the soonest long-window reset.
`CODEXCTL_SELECT=most-available` selects by headroom instead.
Explicit credit-billing selection still requires consent or `--allow-billing` on a non-interactive terminal.
The helper checks billing again before it supplies a token.
If credentials rotate during billing or routing checks, the broker repeats those checks once.
Further rotation refuses delivery until the operator retries with stable evidence.
Organizational plans and accounts with credit evidence need a closed spend cap to qualify for automatic selection.
A missing or open cap for those accounts requires billing consent.
Subscription credits alone do not prove that further spending is disabled.
No remote command redeems a banked reset implicitly.
Finish existing TUI sessions and stop the daemon before switching accounts.
Pending local logins and local recovery wrappers also block remote activation.
Parallel local commands remain supported.
An explicit saved local alias can restore local mode before migration, even when the server is unavailable.
Transferred aliases require the server. A known remote connection with the same alias blocks local selection.
Device login refuses a transferred alias even after disconnect.
`whoami` reports the active account. Retiring live credentials also clears the local active marker.
After disconnect, it reports a local profile only when local credentials remain active.
The native provider currently supports the global ChatGPT backend with no regional routing constraint.
Before each token delivery, the server checks workspace routing.
Regional routes, routing overrides, and missing routing evidence refuse import verification or activation.
Read-only brokers cannot verify native routing and cannot supply the native provider.
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
Its parent application registers it from Git. Automatic sync, prune, and self-heal
then reconcile `deploy/k8s/overlays/staging` from this repository.
The internal ALB terminates HTTPS. The broker uses a ClusterIP service.

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

The SSO overlay uses the company production identity issuer `https://clerk.sawmills.ai`.
Its test mode is disabled. The service deployment and dedicated client remain in staging. A dedicated confidential application requires PKCE and has one exact
HTTPS callback. Google sign-in is enabled. Enrollment requires a verified
`sawmills.ai` primary email. The broker requests only `openid email`.
See [the OIDC configuration research](research/central-staging-oidc.md).

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
Owner failures during account listing have a separate bounded reason.
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
