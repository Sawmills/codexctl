# codexctl coding standards

These standards guide independent review of this repository. Existing repository
and nested instructions still apply.

## Review scope

Apply these standards to changed behavior and the contracts it affects. Existing
violations do not require unrelated cleanup. Report a finding with its location,
trigger, consequence, and smallest supported correction. Style preferences alone
are not correctness findings. A standards exception requires maintainer approval
and a recorded reason,
evidence, owner, and removal condition. It cannot waive existing safety,
authority, or mandatory CI requirements.

## Deep modules and interface design

A deep module owns substantial behavior behind a small interface. Judge depth by
what callers no longer need to know, not by file length or a fixed layer count.
Keep policy beside the state it governs. A wrapper that only forwards parameters
adds little value. Prefer a concrete implementation until a real boundary needs
substitution. Do not introduce repositories, factories, or generic adapters only
for imagined future reuse.

For a material interface change, compare two plausible interfaces before coding.
List caller inputs, outcomes, failure states, cancellation, and side effects for
each. Choose the interface that removes more caller obligations without hiding
necessary information. Keep public module exports narrow. Do not expose private implementation types
merely to shorten call sites.

Test the public behavior through the production path. A test that only proves a
helper works cannot prove that its caller uses it correctly. Keep small pure
functions when they own a useful domain calculation. Do not split a cohesive
operation into fragments just to mock each fragment.

## Identity and credential authority

Treat an account as workspace identity plus login identity. An alias is a local
name, not account proof. Preserve all token claims and compare claims only in
their own namespace. Unreadable data is unknown evidence. Unknown ownership is
not agreement. Follow the exact replacement and adoption rules in AGENTS.md.
Do not broaden `--allow-adopt` into permission to override a proven conflict.

Keep tokens, auth files, and account identifiers out of diagnostic payloads unless
the identifier is necessary and non-secret. Never log full tokens or credential
JSON. Use private files and directories through the existing store helpers.

## Store consistency and process isolation

Use the shared store lock for coupled profile mutations and live switches.
Preserve same-directory temporary files, exclusive creation, synchronization,
atomic replacement, and directory synchronization. A successful rename is not
proof that several files changed as one transaction. Keep crash recovery explicit.
The live auth file and active marker are separate files with an ordering contract.

`codexctl use` remains an offline local auth swap. Pinned exec must not change the
live auth file or active marker. It can provision its own home and capture rotated
auth back to its profile. Preserve existing entries in the shared-home links and
refuse an inherited `CODEX_HOME` as specified. Keep login homes distinct from exec
homes. Do not describe pinned exec as having no filesystem side effects.

## Billing, resets, and recovery

Keep billing approval separate from reset approval. Never auto-select usage-based
accounts during recovery. Preserve non-interactive refusal and explicit flags.
An alias selection does not authorize a banked reset. Redeem only an eligible
reset for an exhausted window and retain the established expiry order.

Preserve idempotency keys across ambiguous reset retries. Do not turn an unknown
remote outcome into a fresh purchase or redemption. Usage data age, fetch success,
model access, and available capacity are different facts. A usage-fetch rate limit
does not prove that an account has no model capacity.

## Rust interfaces and child processes

Prefer concrete domain types and enums for account decisions and failure states.
Use small traits only when an actual external boundary needs substitution. Keep
identity comparison in one owner instead of repeating claim precedence in each
command. Keep transport, storage, and terminal decisions out of pure policy.

For a new command interface, compare a domain request with a collection of flags
that callers must coordinate. Make invalid combinations hard to express without
removing explicit consent. Return useful errors with context and preserve causes.
Do not panic on malformed external data or expected filesystem failures.

Pass child arguments as arguments through `Command`, not as shell source. Preserve
working directory, explicit environment, exit status, and signal behavior. Bound
network waits and retries. Keep async work from blocking unrelated status work.
Do not claim cross-platform filesystem behavior without platform evidence.

## Validation

Use temporary homes and synthetic credentials for tests. Never exercise mutation
tests against the operator's real auth store. For changed lock or atomic-write
behavior, require real filesystem and process tests, including interruption and
concurrent access. For changed PTY or child behavior, execute a real child process.
For changed HTTP handling, use a protocol server and distinguish it from provider
acceptance. A billing or reset test must not spend real credits without explicit
authorization. Preserve Linux and macOS CI coverage, formatting, Clippy, tests,
release build, dependency audit, and Trunk checks.

## Evidence and delivery

Use the repository's locked dependencies and current workflow commands. For a
behavior fix, show a regression that fails without the fix and passes with it.
Do not add tests that merely copy the implementation or inspect static text.
For a changed external contract, exercise the actual dependency or protocol at
that boundary. Label stubbed tests, local checks, hosted CI, deployment, and live
acceptance separately. A green PR does not prove production behavior.

Keep secrets, tokens, personal data, and unbounded payloads out of logs and test
artifacts. Expose actionable failures at their owning boundary. When changing
observability, cover error classification and bounded metric labels. State which
alerts exist and which notification deliveries were actually observed.

Documentation-only changes need link, command, and source checks plus applicable
repository gates. Do not create production side effects to validate prose.

See [the research record](docs/engineering/coding-standards-research.md) for
source observations, decisions, and limitations.
