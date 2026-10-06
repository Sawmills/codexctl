# Plan: Add a New Server Account With `codexctl login`

**Decision.** Replace the WIP client upload with a server-managed device login, the same protocol that renewal uses. The server runs `codex login --device-auth` in a private server home and admits the result through the existing import admission (`Broker::import_account`). The refresh token never reaches the client disk, and `--no-browser` works because the client only prints the code.

## Why Not the WIP Client Upload

The WIP (`9e5a9ee`) runs the device login on the client, keeps `auth.json` in a pending receipt under `~/.codexctl`, and posts it to `POST /v1/accounts`. Three defects follow from that design. A refresh-bearing credential sits on the client until the server confirms it. The flow refuses `--no-browser`. It also treats the import endpoint as a login protocol, although the docs describe import as migration and repair ("Transfer Existing Accounts", "Failure and Recovery"). The WIP's alias routing in `remote::login` (catalog miss plus not-known alias) is correct and stays.

## Design

Server: three routes `POST /v1/accounts/login/{start,status,cancel}` with body `{alias, label?, id}`. The operation record and login home live at `state/account-logins/<account_key(user, alias)>/<id>/`, so aliases stay per-user. The handlers reuse the renewal parts in `relogin/worker.rs`: `challenge`, `spawn_login`, `process::isolate`, the 30-second challenge deadline, the cancel flag, and the device-revocation check. On a clean exit the worker passes the candidate auth to `import_account` with `AdmissionKind::Migration`. Import admission then performs identity, ownership, verification, and owner launch. The worker deletes the login home after a terminal result.

Client: `remote::login` keeps one receipt `.login-<digest(alias)>.json` with `{server, userId, alias, id, kind: "add", label}`. It persists the receipt before `start`, polls `status`, prints the code, and opens the browser unless `--no-browser` is set. `--cancel` with an `add` receipt calls `cancel`. `--label` is valid only for `add`. On `completed`, the client clears the receipt and fetches the catalog.

## B33 (PostgreSQL Mode)

- Gate the new routes with `reject_unshared_workflow("account_login_unavailable")`, the same gate that renewal, enrollment, and resets use today. The login record is replica-local, and a status poll can reach another replica. The client reports "adding accounts is unavailable while the server runs in shared mode".
- The admission step is `import_account` unchanged. Its first refresh owner therefore starts under the import lease and settlement from #133 and the startup lease order from #131. This plan adds no second launch path.
- A shared-mode login record (durable in PostgreSQL) is a later ticket, together with renewal. I need HQ to confirm this scope in the questions file.

## Failure Cases

| Case | Behavior |
|---|---|
| Transport failure after `start` | The receipt keeps `id`. A rerun calls `start` with the same `id`; the server returns the stored record. No second login. |
| Alias already on the server for this user | The client routes to renewal (current code). The server `start` also refuses with 409 `alias_exists`. |
| Account owned by another user | Admission returns 409 `account_already_owned`. The server retains and fences the grant, as renewal does for `wrong_account`. |
| Account owned by this user under alias X | Admission refuses (`clear_registry` denial; exact code to be pinned by a test); the client names X. Recommendation: refuse and name X. AGENTS.md says `login` lands on the existing profile; I ask HQ whether that rule applies here. |
| Same alias pending on another device | 409 `login_belongs_to_another_device`. |
| Cancel | The worker kills the child, records `canceled`, and deletes the home. No account appears. |
| Verification fails | `failed`, no catalog entry, and the grant stays reserved by import admission. |

## Tests (TDD, `tests/central_managed_test.rs` with the protocol fixture)

1. New alias: login completes, `list` on a second device shows it, and the token route serves it.
2. Retry: the client drops after `start`; a rerun with the same receipt finishes with one fixture login.
3. Existing alias: renewal path runs; direct `start` gets `alias_exists`.
4. Other user owns the account: 409 `account_already_owned`; the grant is not in either catalog.
5. Cancel: the record is `canceled`, no account exists, and the home directory is gone.
6. `--no-browser`: the code prints and no browser starts.
7. Shared mode: routes return 503 `account_login_unavailable`.
8. No client file holds a refresh token after any case.

Gates: `cargo fmt --all -- --check`, `cargo clippy --all-targets`, `cargo test --all-targets`. Docs: rewrite the WIP paragraph in `docs/central-server.md`. Live e2e on staging needs Amir to approve the OpenAI device code.
