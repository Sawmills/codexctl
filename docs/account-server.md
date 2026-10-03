# Standalone provider account server

Implementation preview for the claudectl pilot. The standalone `account-server`
binary shares company SSO, machine enrollment, authorization, encrypted storage,
ownership locks, and HTTP lifecycle with codexctl-central. Claude-only installations
do not install Codex and require no OpenAI account.

## Build and deploy

```sh
cargo build --release --locked --bin account-server
# Install only the neutral binary from a reviewed commit:
cargo install --git https://github.com/Sawmills/codexctl --rev COMMIT --bin account-server --locked
# Or build a Claude-only container with no Codex or Node runtime:
docker build -f deploy/account-server.Dockerfile -t account-server:reviewed .
```

The image uses an unprivileged UID/GID 10001. Mount a private writable volume at
`/state` owned by that user and an independently protected key/SSO-secret mount.
The runtime contains the server, matching shared libraries, and CA certificates.
The existing OpenAI Dockerfile and deployment stay available separately.

```sh
account-server setup --state /state/accounts --key-file /keys/vault
account-server serve --state /state/accounts --key-file /keys/vault \
  --listen 0.0.0.0:8787 --public-url https://accounts.example.com \
  --sso-config /configuration/sso.json --providers anthropic
```

Company OIDC configuration has the same shape and callback path as
[the existing deployment](central-server.md): issuer, client ID,
private client-secret file, and allowed company email domain. Register
`https://accounts.example.com/auth/callback` exactly. A non-loopback listener
requires HTTPS ingress and SSO. Forward only from the trusted ingress; never log
Authorization headers or request/response bodies. The server emits sanitized
operational reason codes. `/health` and `/ready` expose no provider credentials.

Run one replica with one durable volume and vault key. The process owner lock is
required even for recovery. Encrypted files are not safe for concurrent writers on
separate restored volumes. Back up the vault key separately from encrypted state;
keep both private. Restoring an older backup does not make its refresh tokens
current. Stop every writer before restoring; uncertain grants require renewal.

For both providers, use `--providers anthropic,openai` and install the pinned
Codex binary required by the existing OpenAI server. Claude-only startup does not
read, repair, or start disabled OpenAI inventory, even when that inventory is bad.

## Machine access and compatibility

Existing `/v1` OpenAI operations retain their protocol. Claude uses `/v2/anthropic`:
accounts, token, usage, migrations/receipt, and login/start + login/complete.
Shared `/v1/me`, devices, revocation and enrollment remain provider-neutral.
An enrollment explicitly requests provider scopes and shows them on the approval
page. Historical registry entries without scopes default to **OpenAI only**.

An administrator can explicitly replace an existing machine's provider scopes:

```sh
account-server grant-provider --state /state/accounts \
  --machine MACHINE_ID --providers anthropic
account-server revoke --state /state/accounts --machine MACHINE_ID
```

A grant does not enable a provider disabled in server configuration. Each request
rechecks the enabled company user, machine revocation and provider scope. Long
operations recheck authorization before delivering their result. Revocation cannot
invalidate already-delivered provider access tokens.

**Upgrade with writers stopped.** The machine registry adds a `providers` field.
Older binaries ignore that field and do not enforce the new scopes: downgrading a
registry containing Claude-only devices would accidentally grant those machines
OpenAI access. Before a downgrade, revoke/remove those machine registrations with
the new binary and inspect the registry. Keep Claude state out of older writers.
Do not roll back the encrypted Claude vault to replay a previous grant.

## Claude ownership and recovery

Claude records are encrypted under
`providers/anthropic/accounts/<account-id>/vault.enc`, schema 2 with an explicit
provider discriminator. The authenticated provider identity is both account UUID
and organization UUID. Another company user or alias cannot reserve the same
identity. Clients receive access token, expiry, scopes, identity, generation and
revision; refresh credentials never appear in token responses.

The account mutex serializes every acquisition. Concurrent rejection of revision A
performs one exchange; other callers receive its successor. The owner records
refresh intent before sending. A timeout or incomplete/rejected response leaves
that intent quarantined across restart; the old refresh token is never replayed.
A successful response is retained encrypted before parsing. The successor is then
persisted before identity verification and delivery. A transient identity failure
can be retried with the saved successor, without another refresh exchange.
Malformed responses remain encrypted for investigation or identity-pinned renewal.

Admission retains its candidate before verification. Retrying the same migration
ID verifies the original candidate, never a different grant supplied by the retry.
A durable receipt prevents a repeated migration from overwriting a successor.
The client must fence old owners before admission; the server cannot discover
unknown machines or backups. Never claim an uncertain inventory is exclusive.

Login uses server-held PKCE and state, bound to company user and enrolling machine.
Exchange intent is persisted before sending the one-time code. A retry may verify
a retained successful response; it cannot replay an uncertain code exchange.
Renewal pins the existing Claude identity, retaining any acquired mismatching grant
for recovery instead of silently replacing the account.

Usage reads share one polling lock, five-minute caches, reset-aware expiry,
one-second request spacing and a global 429 cooldown honoring Retry-After.
`cached=true` does not acquire a token or contact Anthropic. 403/429 are never
interpreted as refresh requests.

## Validation and release gates

Tests cover standalone startup without Codex, disabled-provider storage isolation,
legacy scope denial, explicit scope grant/revocation, concurrent refresh, restart,
uncertain-exchange quarantine, successor verification retry, admission retry,
login-response recovery, and usage sharing. Existing OpenAI tests are run alongside
these checks. The neutral container has been built and smoke-tested for startup,
`/health` and `/ready` without Codex installed.

The live Claude pilot remains pending; synthetic OAuth headers are not proof of
subscription billing. Do not publish a release before the client launch boundary,
native Mac Keychain behavior, actual expiry/rotation, ownership inventory, and live
entitlement checks pass. The implementation PRs remain drafts until then.
