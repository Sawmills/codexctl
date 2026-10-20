# codexctl

Manage multiple OpenAI Codex CLI accounts. Keep credentials on a private account
server or in local profiles, check usage, and launch Codex with account recovery.

## Install

Install [Codex CLI](https://developers.openai.com/codex/cli) first, then codexctl:

```sh
brew install Sawmills/tap/codexctl
```

Or build from source with Rust:

```sh
cargo install --git https://github.com/Sawmills/codexctl --locked
```

[Release downloads](https://github.com/Sawmills/codexctl/releases/latest) include
macOS and Linux binaries for arm64 and x86_64.

## Quick Start

If your team has an account server, ask its operator for the HTTPS URL. Connect
this machine, sign in with company SSO, and compare the browser and terminal codes:

```sh
codexctl connect --server https://YOUR-SERVER
```

Then check your server accounts, activate one, and launch Codex:

```sh
codexctl status
codexctl use
codexctl codex
```

Add a server account or renew its OpenAI login with `codexctl login work`, where
`work` is its alias. To move existing local profiles, follow the
[migration steps](docs/central-server.md#transfer-existing-accounts) after stopping
all previous credential owners. Account-server credentials stay on the server;
Codex runs tools on your machine and sends inference requests directly to OpenAI.

For accounts kept only on one machine, use the
[local profile guide](docs/local-profiles.md).

## Check Usage

```sh
codexctl status
codexctl list
codexctl whoami
codexctl resets
```

[Read the status columns](docs/status.md), use the
[JSON output](docs/status-json.md) for automation, or add cached usage to a
[terminal status bar](docs/statusline.md).

## Choose an Account

```sh
codexctl use work
codexctl switch
```

`use` changes the machine's active account. To pin one server-account session
without changing other sessions, select its alias at launch:

```sh
codexctl codex --account work
codexctl codex --account work resume SESSION_ID
```

Resume other saved sessions with `codexctl codex resume SESSION_ID`. See
[launching and recovery](docs/launching.md) for billing approval, account selection,
and local state maintenance. Usage-based accounts are never selected automatically.

## Apply a Banked Reset

```sh
codexctl resets
codexctl resets --redeem work
```

Redemption requires an exhausted usage window and uses the qualifying reset closest
to expiry. Explicit account selection never redeems a reset. Reset approval and
billing approval are separate. See [banked resets](docs/resets.md).

## Manage Local Profiles

On a machine without an account-server connection:

```sh
codexctl login work
codexctl use work
codexctl exec --account work -- codex
```

`login` uses an isolated Codex home. `use` swaps the local auth file. `exec` gives
one child its own pinned home and preserves the active profile. On a connected
machine, `login` uses the server; `login --local work` saves a new local profile
instead. See [local profiles](docs/local-profiles.md) for labels, identity checks,
daemon restarts, and shell completions.

## More Guides

- [Account server setup, migration, and daily use](docs/central-server.md)
- [High availability and deployment](docs/central-ha.md)
- [Browser account dashboard](docs/web-dashboard.md)
- [Loopback relay for capacity stops](docs/relay.md)
- [Domain glossary](GLOSSARY.md) and [architecture decisions](docs/adr/)

Run `codexctl --help` or `codexctl COMMAND --help` for the installed command syntax.

## License

[Apache-2.0](LICENSE)
