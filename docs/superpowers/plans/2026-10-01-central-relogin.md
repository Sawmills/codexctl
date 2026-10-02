# Renew OpenAI Login on the Server

## Scope

Use the existing `codexctl login <alias>` for an existing server account.
The user opens the OpenAI device page and approves the code in their browser.
The server receives and stores the replacement credentials.
Connected machines keep their account alias and device registration.
The command does not change local OpenAI credentials or the active account.

## Interface Choice

An upload-based repair requires the caller to create, protect, and retire local credentials.
It also creates a second place that can refresh tokens.
A server-managed device login removes those caller duties.
Route registered server aliases through `login`; preserve local login when no server registration exists.

The command starts or resumes a durable operation for the selected alias.
It displays a verified OpenAI URL and a bounded one-time code.
It polls until the server reports completion or an actionable failure.
`--cancel` stops the current operation for that alias.
`--no-browser` supports a terminal without a browser.
Repeated start requests resume the same operation while it is pending.
Status and cancellation require the account owner's company device credential.
Another enrolled device under that same company identity can resume the operation.

## Native Login

The server pins Codex `rust-v0.159.0`, source commit `687a119f0fcaace47e1f1abcc77cec6c813fd6da`.
Its App Server supports `chatgptDeviceCode` and login completion notifications.
However, a successful login reloads account data before it emits that notification.
That reload can refresh the newly issued grant before the broker checks its identity.
Do not use that path for an isolated candidate.

Use native `codex login --device-auth` in a new empty private home.
That command saves the grant and exits without an account-reader refresh.
It must never reuse or seed a login home because login first logs out its previous grant.
Capture its output privately, with a size limit and a server deadline.
Extract only the expected prompt fields; never forward raw output or error messages.
Accept only `https://auth.openai.com/codex/device` and a bounded printable code.

Do not force a workspace during native login.
The pinned command checks that constraint after token exchange and can discard a wrong-account grant.
Retain the candidate instead, then compare workspace, subject, and every previously known login UID.
Each claim must agree in its own namespace.

## Ownership and Persistence

Serialize preparation and replacement with the existing import lock.
Persist an operation intent before stopping the current owner.
Stop that owner, confirm process exit, and save its latest journal.
Mark the selected account unavailable throughout sign-in.
Unrelated accounts remain usable.

Use a unique private candidate directory for each operation.
Record the exact login process incarnation before publishing its challenge.
A server deadline, cancellation, device revocation, or graceful shutdown stops and waits for that process.
Disconnecting the client does not discard an operation.
Never start another candidate while process ownership remains unknown.

After confirmed login-process exit, read and validate the candidate.
Copy it into an atomic, synchronized private record before promotion.
The native login file itself is not an atomic persistence guarantee.
Preserve wrong-account grants in quarantine and leave the selected account unchanged.
Stop any known owner whose grant matches that quarantine.
Reject overlapping imports until the rightful account owner supplies a matching fresh login.
Never assign a wrong-account grant to a different company user.

For a matching candidate, persist a commit intent with the original identity and exact old/new credential digests.
Write the account journal before the encrypted vault.
Resolve that intent before ordinary timestamp-based journal recovery on restart.
Restart and verify the native owner only after the candidate process has exited and identity checks pass.
Report success only after routing, native login verification, and durable credential storage succeed.

## Failure and Recovery

A stopped child does not prove that OpenAI undid an issued grant.
A canceled, interrupted, or incomplete sign-in keeps the selected account unavailable.
Preserve its old credentials and operation evidence for a safe retry.
A live or unidentifiable process blocks replacement.
A completed wrong-account login can revoke that wrong account's previous grant at OpenAI.
The server cannot prevent this issuer side effect after browser approval.
Show the selected alias before opening the browser and explain this risk.
Do not claim that cancel restores prior OpenAI authorization.

## Verification

Test the public HTTP and CLI paths with a real server process and synthetic native login children.
Cover same-identity replacement, shared-workspace wrong login, UID loss/conflict, cross-user access,
duplicate start, two-device resume, cancellation, timeout, revocation, and unrelated account use.
Cover crash windows before candidate spawn, after grant save, and between journal/vault promotion.
Use real filesystem and process evidence for ownership and persistence tests.
Prove the gap with a failing regression before implementation.
Run formatting, Clippy, default and no-default-feature tests, release build, Trunk, and independent Astra review.
Report protocol fixtures separately from a live OpenAI acceptance test.

## Primary Sources

- [Official App Server authentication documentation](https://learn.chatgpt.com/docs/app-server#authentication-endpoints).
- [Pinned App Server account processor](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/app-server/src/request_processors/account_processor.rs).
- [Pinned native CLI login](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/cli/src/login.rs).
- [Pinned device-code login](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/login/src/device_code_auth.rs).
- [Pinned auth storage](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/login/src/auth/storage.rs).

The plan was checked against pinned source by a read-only research agent before implementation.
