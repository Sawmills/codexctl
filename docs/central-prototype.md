For company SSO, multi-user accounts, migration, and Kubernetes setup, use [the central server guide](central-server.md). This document describes the earlier single-account prototype. Central support is now enabled by default.

# Central Account Prototype

This prototype lets two registered clients run Codex locally with one server-owned account.
The server holds the refresh token and gives clients only access tokens.
The native TUI uses command-backed provider authentication.
An earlier one-prompt client also uses the experimental Codex App Server interface.
The prototype supports one account, multiple devices, and explicit tenant and user ownership.
Use the Native TUI Workflow section for the regular Codex terminal interface.

## Build and Test

```sh
cargo build --features central-prototype --bin codexctl-central
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

Central support is enabled by default. `--no-default-features` builds only the local account commands.
The normal codexctl commands keep their current behavior.
The automated tests use a synthetic Codex process for protocol and refresh failure scenarios.
They do not prove live OpenAI refresh behavior.

## Prepare the Server

Before normal server use, transfer exclusive refresh ownership of the imported login to the server.
Other Codex processes must not retain the same refresh token.
The prototype does not remove or revoke existing copies.
A dedicated server login is the safer input for a real deployment.

For a compatibility experiment with an existing fresh login, use `serve --read-only`.
This mode issues the current access token but refuses refresh.
It starts no central Codex owner process, so that process cannot rotate the imported login.
It does not prove that the server can maintain a live login over time.

Keep the key file separate from server state and encrypted backups.
The key destination must be a new file.
The auth source must be a private regular file with mode 0600 or stricter.

```sh
mkdir -m 700 ./central-private
./target/debug/codexctl-central init \
  --state ./central-private/server \
  --key-file ./central-private/vault-key \
  --auth /absolute/path/to/server-login/auth.json \
  --alias personal --tenant personal --user amir
```

The encrypted vault uses AES-256-GCM with a random nonce for each write.
The implementation reuses the RustCrypto library and codexctl's private atomic file writes.
The server decrypts credentials into a private runtime directory because Codex maintains a file-based login.
The server encrypts the current credentials before it returns a token response.
After successful encrypted persistence, a graceful shutdown removes the runtime directory.
After the broker reports its listening address, SIGINT, SIGTERM, and SIGHUP use this shutdown path.
The server waits for the owner process to exit before its final snapshot and runtime removal.
During the initial owner handshake, an operator interrupt acts as an abrupt stop and retains the runtime for recovery.
A failed save or unavailable owner retains the runtime directory for recovery.
After an abrupt shutdown, the server refuses a restart while that directory remains.
The operator must recover its refreshed credentials before restart, because the encrypted vault can hold an older token.
This prototype has no automated crash recovery or key rotation.

## Register Clients

Run device registration on the server.
Each device receives a separate credential file.
The registry stores only its SHA-256 hash.
The server denies access when the device tenant or user differs from the account owner.

```sh
./target/debug/codexctl-central register \
  --state ./central-private/server --device laptop \
  --tenant personal --user amir --token-file ./central-private/laptop.token
./target/debug/codexctl-central register \
  --state ./central-private/server --device desktop \
  --tenant personal --user amir --token-file ./central-private/desktop.token
```

Copy each token file securely to its assigned machine.
No OpenAI refresh token leaves the broker.
An access token still grants access to the account until it expires.
Device revocation stops future broker access but cannot revoke an access token already issued by OpenAI.

## Run the Broker and Clients

```sh
CODEXCTL_ALLOW_INSECURE_LOOPBACK=1 ./target/debug/codexctl-central serve \
  --state ./central-private/server --key-file ./central-private/vault-key
```

This earlier workflow runs the broker on loopback.
Its HTTP examples require the explicit `CODEXCTL_ALLOW_INSECURE_LOOPBACK=1` test opt-in.
The client rejects redirects and requires HTTPS for network origins.
For the earlier cross-machine experiment, an SSH tunnel reached the loopback listener.
Use the central server guide for the current HTTPS workflow.

```sh
ssh -N -L 8787:127.0.0.1:8787 server-host
```

On each client, run the prototype from the project directory.
The client keeps the current directory and starts a private local Codex process.
The prototype uses a read-only sandbox and does not approve interactive tool requests.

```sh
CODEXCTL_ALLOW_INSECURE_LOOPBACK=1 codexctl-central run \
  --server http://127.0.0.1:8787 --token-file /absolute/path/laptop.token \
  'Explain the README. Do not change files.'
```

The client starts one conversation and prints the completed response.
The prototype does not support interactive terminal sessions or session resumption.
The local client uses in-memory external authentication and refuses success if Codex writes an `auth.json` file.

## Refresh, Revocation, and Failures

After an authorization error, App Server requests a new access token from the client.
The client sends its account identifier and token revision to the broker.
The broker serializes refresh calls through one Codex owner process.
When two clients submit the same revision, the second receives the first refresh result without another rotation.
An operating system file lock prevents two broker processes from owning the same state.
This is one server process, not a distributed lock across several servers.

```sh
codexctl-central revoke --state ./central-private/server --device laptop
```

The registry reloads on every token request, so revocation needs no restart.
The broker fails closed after an owner protocol failure or an uncertain refresh timeout.
A disconnected client cannot cancel the server refresh task.
An owner RPC timeout leaves the Codex process alive so it can finish a credential write.
Client HTTP requests have an eight-second deadline because OpenAI gives the external-auth callback about ten seconds.
The owner can wait up to ninety seconds for refresh without interruption from the client.
A slow refresh can finish on the server after the client fails.
A later client run can use that result, but the prototype does not resume the failed turn automatically.
It does not retry a refresh whose completion is unknown.
Errors contain bounded reasons instead of protocol payloads or credentials.
The authenticated `/metrics` endpoint counts failed broker requests by reason.
Structured failure logs include operation, stage, reason, and HTTP status.
No monitoring collector, alert rule, or notification route exists in this local prototype.

## Live End-to-End Experiment

```sh
CODEXCTL_ALLOW_INSECURE_LOOPBACK=1 python3 tests/central_live_e2e.py \
  --binary target/debug/codexctl-central \
  --auth /absolute/path/to/fresh/auth.json
```

This experiment imports a fresh login into a temporary encrypted vault.
It starts a real broker and two real local Codex clients at the same time.
It requires two exact model replies and unchanged source credentials.
Live refresh stays disabled to protect the original login.
The temporary broker, key, device secrets, and runtime directories are removed afterward.
The experiment uses included account quota and needs a usable account with enough quota.
It exercises two independent client processes on one physical machine.
This earlier experiment does not cover a second machine or live refresh.
The native workflow tests below cover those cases with a dedicated login.

## Sources

- [OpenAI App Server authentication](https://learn.chatgpt.com/docs/app-server)
- [OpenAI refresh ownership guidance](https://learn.chatgpt.com/docs/auth/ci-cd-auth)
- [Axum HTTP routing](https://docs.rs/axum/0.8.9/axum/)
- [RustCrypto AES-GCM](https://docs.rs/aes-gcm/0.10.3/aes_gcm/)

OpenAI marks external ChatGPT authentication as experimental.
The implementation uses the documented integration point instead of a custom OAuth refresh endpoint.

## Native TUI Workflow

Build both prototype binaries with `cargo build --features central-prototype --bins`.
After server setup and device registration, connect each device once:

```sh
CODEXCTL_ALLOW_INSECURE_LOOPBACK=1 ./target/debug/codexctl-central connect --alias personal \
  --server http://127.0.0.1:8787 --token-file /absolute/path/device.token
CODEXCTL_ALLOW_INSECURE_LOOPBACK=1 ./target/debug/codexctl use personal
CODEXCTL_ALLOW_INSECURE_LOOPBACK=1 codex
```

With one registered remote account, `codexctl use` also selects it without an alias.
Automatic selection refuses usage-based accounts and unknown billing.
An explicit selection requires billing confirmation for those accounts.
The broker reads effective rate limits and credits before it returns native credentials.
A changed billing plan or billing class requires new approval.
A failed billing read returns no token. A slow read can outlast the client deadline.
The normal Codex TUI and local tools run unchanged.
Codex calls a private `codexctl central-token` helper through its command-backed provider authentication.
The helper gets access tokens from the broker and never stores an OpenAI refresh token on the device.
Each helper invocation requests server-side refresh; this includes startup calls.
Codex caches the result for the session and calls the helper again after an authorization error.
The helper has a 195-second HTTP deadline, and Codex has a 210-second command deadline.
These deadlines differ from the experimental App Server callback used by the earlier one-prompt client.

Activation changes the model provider in the local Codex configuration.
It refuses a default Codex profile that overrides the provider.
It preserves the local login file and unrelated configuration.
The provider supports the global ChatGPT Codex endpoint with the assigned account header.
Regional routes and missing routing evidence refuse native activation or token delivery.
Read-only mode supports only the earlier App Server client because it cannot verify native routing.
Codex shows a custom provider instead of the standard ChatGPT account display.
This prototype has no remote account availability ranking.
Switching to a local profile restores the previous provider before swapping its login.
After the daemon stops, run `codexctl-central disconnect` to restore the previous provider without switching the local login.
Disconnect also refuses a running daemon.
If the selected provider changes, disconnect preserves that user choice and removes the stale remote marker.
If the TOML file cannot be parsed, fix its syntax before disconnecting.
The marker is `~/.codexctl/central/.native-active.json`.
A failed activation removes its newly prepared marker; an abrupt stop can leave one that disconnect clears.
Linked configuration files keep their links and file permissions. Their parent directories keep their permissions.
Remote activation refuses inherited Codex homes and pinned aliases.
While the remote provider is selected, local login, save, switch, exec, and recovery commands refuse instead of reporting or using a different account.
The token helper also refuses an existing pinned shell that sees the remote provider through its linked configuration.
Use regular `codex`, or disconnect before those local account wrappers.
Already-running TUI sessions keep the connection selected when they started.
Remote activation refuses a running Codex daemon.
Finish its sessions and run `codex app-server daemon stop` before selection.
The prototype does not migrate existing sessions.

## Native Workflow Test Evidence

On September 29, 2026, the native workflow passed on two physical Macs through an SSH tunnel.
The desktop used Codex 0.159.0. The MacBook used Codex 0.156.1.
Each machine ran `codexctl use` and the regular Codex TUI with its default daemon.
Each TUI read a different local file and returned its exact contents.
Neither client created an OpenAI login file or received a refresh token.

A separate test injected one HTTP 401 during the second turn of the same TUI session.
The native helper requested a real server-side token refresh, and the access token changed.
The same TUI then completed its next reply through OpenAI.
This test exercises authorization-error recovery. It does not wait for literal token expiry.
A graceful broker restart also preserved the latest access token in the encrypted vault.

The repeatable client test uses an already running broker and a registered device:

```sh
CODEXCTL_ALLOW_INSECURE_LOOPBACK=1 python3 tests/central_native_e2e.py --bin-dir target/debug \
  --server http://127.0.0.1:8787 --device /absolute/path/device.token \
  --receipt /tmp/native-receipt.json
```

Live tests use a separate server login and consume included account quota.
CI uses synthetic credentials and does not run these live tests.

To unregister a remote alias after disconnecting, remove its `~/.codexctl/central/<alias>.json` file.
Disconnect keeps this registration, so `codexctl use` continues to select the single registered remote account.
Inline provider tables can become standard TOML tables during activation. Disconnect preserves their values but does not restore their inline layout.
