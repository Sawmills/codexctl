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
Only the initiating device can resume or cancel a pending operation.
Cross-device resume is deferred to a follow-up.

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
A live or unidentifiable process blocks replacement unless Linux parent-death protection proves that the recorded broker exit also stopped the child.
A completed wrong-account login can revoke that wrong account's previous grant at OpenAI.
The server cannot prevent this issuer side effect after browser approval.
Show the selected alias before opening the browser and explain this risk.
Do not claim that cancel restores prior OpenAI authorization.

## Verification

Test the public HTTP and CLI paths with a real server process and synthetic native login children.
Cover same-identity replacement, shared-workspace wrong login, UID loss/conflict, cross-user access,
duplicate start, same-device resume, other-device refusal, cancellation, timeout, revocation, and unrelated account use.
Cover crash windows before candidate spawn, after grant save, and between journal/vault promotion.
Use real filesystem and process evidence for ownership and persistence tests.
Prove the gap with a failing regression before implementation.
Run formatting, Clippy, default and no-default-feature tests, release build, Trunk, and independent Astra review.
Report protocol fixtures separately from a live OpenAI acceptance test.

## State Machine and Crash Recovery

This table is the implementation contract. Each operation has one private `record.json`.
The record contains its sequence, initiating device, phase, process evidence, candidate,
old/new credential digests, error, and retirement marker. Each transition replaces that
record with one atomic, synchronized write. A new operation becomes visible by renaming
a fully initialized temporary directory. The highest published sequence selects the
current operation; no separate current-operation pointer is needed.

| Phase and crash point                                       | Durable evidence on disk                                                                                                         | Recovery action                                                                                                                               | Terminal or resumable state                                                                    |
| ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| Preparation before directory publication                    | Only an unpublished temporary directory; no child can have started                                                               | Ignore the temporary directory                                                                                                                | No operation                                                                                   |
| Prepared before stopping the old owner                      | Published record says no login child started                                                                                     | Confirm the old owner stopped; retain credentials and mark interruption                                                                       | Failed, retry allowed                                                                          |
| Spawning before process evidence is saved                   | Record says spawn attempted but has no process identity                                                                          | On Linux, prove the recorded broker incarnation exited and use parent-death protection as child exit proof; otherwise retain the global fence | Failed on Linux after proven broker exit; process reconciliation on other systems              |
| Pending with a live or unidentifiable child                 | Record contains process incarnation, or process inspection is inconclusive                                                       | On Linux after broker exit, use parent-death protection; with a live broker or on other systems, retain the global fence                      | Recover the native file after proven exit; otherwise Pending                                   |
| Pending after confirmed child exit, no complete native auth | Process exit is proven; native file is absent or partial                                                                         | Retain native bytes; mark interruption; fence only the selected account                                                                       | Failed, retry allowed                                                                          |
| Pending after complete native auth save                     | Process exit is proven; native file contains a complete grant                                                                    | Atomically capture candidate and commit intent after identity and ownership checks                                                            | Committing, resumable                                                                          |
| Pending after wrong-account auth save                       | Process exit is proven; grant disagrees with selected identity                                                                   | Atomically retain candidate and failure; reserve the observed identity                                                                        | Failed, rightful owner can repair                                                              |
| Committing before journal write                             | Record contains candidate and exact old/new digests; vault still contains old grant                                              | Validate intent and identity; replay journal and vault writes                                                                                 | Promoted, resumable                                                                            |
| Committing after journal write, before vault write          | Same intent; journal contains candidate; vault contains old grant                                                                | Replay the same candidate into vault; preserve intent on error                                                                                | Promoted, resumable                                                                            |
| Committing after vault write, before phase write            | Same intent; journal and vault contain candidate                                                                                 | Verify exact digests; finish phase write                                                                                                      | Promoted, resumable                                                                            |
| Promoted, verifier spawning                                 | Record contains verifier spawn intent and current broker incarnation before runtime process evidence is cleared                  | On Linux, prove that broker incarnation exited and use parent-death protection; otherwise retain only the selected account fence              | Resume verification on Linux after proven broker exit; process reconciliation on other systems |
| Promoted before or during native verification               | Candidate and intent remain; journal can contain a rotated grant                                                                 | Prove old process exit, reconcile journal, then verify with a writable server                                                                 | Promoted, retryable; Failed only after definitive rejection is reconciled                      |
| Promoted after permanent rejection                          | Native rejection is durable and refers to the unchanged verification input, which can be a rotated grant; process exit is proven | Retain grant and rejection evidence; allow a new explicit login                                                                               | Failed, retry allowed                                                                          |
| Retiring during reservation updates                         | Record proves full verification; some matching stopped reservations may already be retired                                       | Repeat retirement under the import lock; reload each reservation before writing                                                               | Completed after all retirement writes succeed                                                  |
| Completed after success response is lost                    | Durable completion and verified vault                                                                                            | Return the saved result to the initiating device                                                                                              | Completed                                                                                      |
| Failed or Canceled after a worker stops                     | Record retains outcome, candidate if readable, and process evidence                                                              | Preserve evidence; ignore unusable candidate bytes after proven exit                                                                          | Failed or Canceled, retry allowed                                                              |
| Corrupt published record with proof no child started        | Private no-start publication evidence; no runtime process uncertainty                                                            | Fence this account only; retain corrupt bytes                                                                                                 | Account requires repair; unrelated accounts remain usable                                      |
| Any recoverable phase on a read-only server                 | Existing intent and process evidence                                                                                             | Perform disk reconciliation only; never launch or verify an owner                                                                             | Failed or resumable phase; writable retry completes verification                               |

The Linux broker uses a current-thread Tokio runtime. Native children spawn directly
on that long-lived main thread, never in the retiring blocking pool. The child sets
`PR_SET_PDEATHSIG=SIGKILL` before exec and checks the parent PID to close the setup
race. Each operation records the broker incarnation before spawn. Linux recovery
uses its proven exit as child exit evidence, including the spawn-to-PID-record gap.

The file approach remains suitable because each phase transition has one authoritative
atomic record write; journal, vault, and reservation writes are idempotent effects of a
durable intent, so SQLite would not make those external effects transactional.

Import, renewal, and startup use one read-only identity inventory. It reads the vault,
runtime journal, retained login candidates, and process-exit evidence before filtering
by identity. An unreadable source remains unknown evidence. A conflicting journal
reserves both the vault identity and the journal identity.

Two interfaces were considered: an async broker inventory that also stops children,
and a read-only disk inventory with explicit process states. The latter supports both
live requests and startup before RPC owners exist. Its callers hold the import lock
or startup owner lock. Import can stop a conflicting RPC and reread the inventory;
renewal refuses while that process remains live or unidentified. Stopped login
quarantines remain eligible for rightful-owner repair only after fresh-grant verification.

All mutation paths take the import lock before resolving the normalized alias and
current owner. Worker completion, cancellation, import, and renewal follow this order.
The lock covers ownership checks and commit verification. An unfinished operation
reserves both its selected identity and any complete candidate identity. A rejected
alias cannot renew an account that another alias or company user already owns.
A waiting worker reloads its record under the lock and honors a retirement written by
another operation. A stopped process does not prove that its worker has finished.

A same-device retry resumes the saved operation. Other devices cannot read its code,
cancel it, or resume it. Cross-device resume is a follow-up after this PR.
The client holds an alias-specific lock only while changing its operation receipt.
Cancellation can run while the first client is polling. Completion removes a receipt
only when it still names that operation.

### Review Regression Coverage

Retain all fifteen reviewer findings as regression cases. The table covers partial
publication, incomplete native writes, commit errors, interrupted login, read-only
startup, permanent rejection, and retirement ordering. Additional cases cover early
import reservation, restoration of writable refresh ownership, rejected-alias
ownership, owner-map replacement while waiting for the lock, late worker retirement,
concurrent CLI cancellation, relative state paths, and normalized remote aliases. Existing tests
cover unchanged import and local-login contracts; new tests exercise the changed
server renewal paths. Record each red/green result against the WIP commit.

## Primary Sources

- [Linux parent-death signal and spawning-thread lifetime](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html).
- Tokio 1.52.3, pinned in `Cargo.lock`: `process::Command::spawn` directly calls `self.std.spawn()` on the calling thread (`src/process/mod.rs:863`).

- [Official App Server authentication documentation](https://learn.chatgpt.com/docs/app-server#authentication-endpoints).
- [Pinned App Server account processor](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/app-server/src/request_processors/account_processor.rs).
- [Pinned native CLI login](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/cli/src/login.rs).
- [Pinned device-code login](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/login/src/device_code_auth.rs).
- [Pinned auth storage](https://github.com/openai/codex/blob/687a119f0fcaace47e1f1abcc77cec6c813fd6da/codex-rs/login/src/auth/storage.rs).

The plan was checked against pinned source by a read-only research agent before implementation.
