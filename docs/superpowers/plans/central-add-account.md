# Plan: Add a New Server Account With `codexctl login`

**Decision.** Replace the WIP client upload with a server-managed device login, the same protocol that renewal uses. The server runs `codex login --device-auth` in a private server home. A new identity goes through the existing import admission (`Broker::import_account`); an identity this user already holds under alias X lands on X through X's renewal path. The refresh token never reaches the client disk, and `--no-browser` works because the client only prints the code. HQ approved this plan with changes C1 to C6 on 2026-10-06.

## Why Not the WIP Client Upload

The WIP (`9e5a9ee`) runs the device login on the client, keeps `auth.json` in a pending receipt under `~/.codexctl`, and posts it to `POST /v1/accounts`. A refresh-bearing credential sits on the client until the server confirms it, and the flow refuses `--no-browser`. The docs describe import as migration and repair, not as a login protocol.

## Server

Routes: `POST /v1/accounts/login/{start,status,cancel}` with body `{alias, id, label?}`, in `src/central/relogin/add.rs` so they reuse the renewal parts (`Record`, `publish`, `challenge`, `spawn_login`, `process::isolate`, the cancel flag, and the device-revocation check).

Add records live at `state/account-logins/<account_key(user, alias)>/relogin/<id>/`, beside `accounts/`, so a vault-less directory never enters the account registry. **C1:** `clear_registry`, `retire_reservations`, and broker startup also inventory this root. A stopped add record with a candidate reserves its identity against imports and renewals, and startup fences any owner that overlaps it. A live or unidentifiable login child blocks replacements, the same as under `accounts/`. Startup turns an exited child with a saved grant into a resumable record (`verifying`), and an exited child without a grant into `failed` (`login_interrupted_retry`).

Start order, all under the imports lock:

1. **C2:** look up the request id first. A known id returns its record; a non-terminal record with an exited child and a saved grant resumes admission. No second login starts.
2. Refuse an alias this user already has with 409 `alias_exists`.
3. **C4:** a non-terminal record for this alias from another device returns 409 `login_belongs_to_another_device`. From the same device, a live record is returned unchanged, so the client adopts its id. One alias has at most one non-terminal record.
4. Publish the record and spawn the login worker.

Admission after the child exits with a grant, under the imports lock:

- **C3, same user holds the identity under X:** settle X's owner, publish a renewal record with the grant in X's state, then run `check_claim`, `promote`, and `verify_replacement`. X keeps its label. The add record retires, records `landed = X`, and reports X's renewal phase with `landedAlias`.
- **New identity:** retire the add record's own reservation, then call `import_account` with the worker's permit and the held imports guard (split into a locked variant). Its first refresh owner therefore starts under the import lease and settlement from #133.
- **Refusals** (`account_already_owned`, `alias_identity_conflict`, `relogin_reserved`): the record fails and the server deletes the grant. **C6:** no fence runs, so another user's verified account keeps serving tokens.
- **Transient failure** (503 classes): the record stays `verifying` with an error and keeps its grant; rerunning the same command resumes admission.

Cancel and device revocation kill the child, delete the login home, and leave no candidate (**C6**).

## Client

`remote::login` reads the add receipt `.login-<digest(alias)>.json` (`kind: "add"`, `label`) **before** catalog routing (**C2**), so a lost `completed` response resumes the same operation. The add flow shares the renewal poll loop with the add endpoints, prints the code, and opens the browser unless `--no-browser` is set. `--label` is valid only for an add; a receipt with another label refuses. On `completed`, the client clears the receipt and refreshes the catalog; with `landedAlias`, it prints that the account is X.

## PostgreSQL Mode (C5, Q1)

The routes return 503 `account_login_unavailable` in non-File modes, through `reject_unshared_workflow`, the same as renewal. This is a known gap: add records and renewal records are replica-local. HQ records "renewal and add in PostgreSQL mode" as a B33 prerequisite. The docs state the gap.

## Tests (TDD, `tests/central_managed_test.rs`, protocol fixture)

1. New alias completes; a second device lists it; the token route serves it.
2. Retry: a second `start` with the same id after `completed` returns `completed`; a CLI rerun with the receipt and the alias already in the catalog finishes without a second login.
3. Existing alias: `start` returns 409 `alias_exists`.
4. Another user owns the identity: `failed` with `account_already_owned`; that user's account keeps serving. Another user's identical alias name does not interfere.
5. Cancel: `canceled`, no account, no login home, no candidate.
6. Device revocation mid-login cancels and deletes the home.
7. C1: kill the broker after the child saves its grant; after restart, the overlapping owner is fenced and `start` with the same id completes without a second login.
8. C3: approving an identity held as `personal` completes with `landedAlias: personal`, adds no alias, and `personal` keeps serving.
9. C4: two ids for one alias on one device return one record; another device gets 409.
10. CLI with `--no-browser`: the code prints, the account appears, and no client file holds a refresh token.
11. PostgreSQL mode returns 503 `account_login_unavailable` (`tests/central_ha_startup_test.rs`, runs in CI).

Gates: `cargo fmt --all -- --check`, `cargo clippy --all-targets`, `cargo test --all-targets`. Live e2e on staging needs Amir to approve the OpenAI device code.
