# Plan: Rename a Server Account Alias

**Decision.** Add `codexctl rename <old-alias> <new-alias>` and `POST /v1/accounts/rename {alias, newAlias}`. The server moves the account to the key of the new alias under an intent journal, keeps its credentials, refresh owner identity, and label, and records a tombstone so a client that still uses the old alias gets a clear `account_renamed` error instead of a silent switch. File mode only, like renewal and add. First real use: `erez+1@sawmmills.ai` to `erez+1@sawmills.ai`.

## Why a Move

`account_key(user, alias)` names the account directory, the `Owners` map key, and `CredentialRecord.account_id`. Startup accepts a directory only when its name equals `account_key(vault.user, vault.alias)` (`managed.rs` startup loop) and many paths recompute the key from `vault.alias`. Keeping the old key would mean changing every one of those sites; moving the account keeps the existing invariant.

## Server

Under the imports lock, after `authorize` and `reject_unshared_workflow("account_rename_unavailable")`:

1. Resolve `alias` for this user (404 `account_not_found`). Refuse 409 `alias_exists` when `newAlias` already resolves for this user (case-insensitive), and 409 `login_pending` when the account has a non-terminal renewal record, an add record under either alias exists that is not terminal, or `import_settling` is set. A case-only change (`Erez` to `erez`) is allowed.
2. Settle the owner as renewal does: fence, `settle_and_stop` or prove exit, `snapshot`.
3. Write `rename.json {user, from, to}` in the old directory, then save the vault with `alias = to`, then rename `accounts/<old key>` to `accounts/<new key>` (one directory rename on one filesystem), then remove the intent.
4. Re-key the `Owners` entry, relaunch the owner through the restore clearance, and add a tombstone `{user, from, to}` to `state/renames.json` (dropped when an alias is reused).

Startup completes an interrupted rename before its inventory: a directory with `rename.json` finishes the remaining steps (vault alias, directory move, intent removal). The reset journal moves with the directory; its stored `upstream_id` keeps retries idempotent.

`/v1/token`, renewal, and reset requests that name a tombstoned alias return 404 `account_renamed` with `{alias: to}`.

## Client

- `codexctl rename` requires a server connection and calls the endpoint, then refreshes the catalog.
- On the renaming machine, `<old>.json` becomes `<new>.json` with the new alias, and `.active-account` follows when it named the old alias. A pending add or renewal receipt for either alias refuses the rename locally.
- Other machines see the new name at their next catalog read. Their active session or pinned lane that still names the old alias fails at the next token refresh with "server account <old> was renamed to <new>; run codexctl use <new>" (or relaunch with `--account <new>`). Nothing switches accounts silently.
- `codexctl codex --account <old>` fails at launch with "server account <old> not found; run codexctl list (it may have been renamed)"; the exact renamed message comes from the token route, which a running lane hits on refresh.

## PostgreSQL Mode

The route returns 503 `account_rename_unavailable`, the same gate as renewal and add. This is a known gap under the B33 prerequisite.

## Tests (TDD)

1. Rename keeps credentials, label, and refresh owner; the token route serves the new alias and a second device lists it.
2. The new alias is taken by this user: 409 `alias_exists`; another user's same alias does not interfere.
3. A pending renewal or add blocks the rename with 409 `login_pending`.
4. A crash after the intent and vault write: restart completes the move and serves the new alias.
5. The active alias on the renaming machine: pointer and connection follow; `whoami` shows the new alias.
6. A pinned lane or another machine using the old alias gets `account_renamed` with the new name, never another account.
7. Case-only rename succeeds.
8. PostgreSQL mode returns 503 `account_rename_unavailable` (CI).
