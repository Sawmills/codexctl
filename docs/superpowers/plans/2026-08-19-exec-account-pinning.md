# PLAN: Account-pinned, non-mutating launch (`codexctl exec`)

Ticket context: SAW-9631 selected-account mismatch. `codexctl use <alias>` mutates global state
(`~/.codex/auth.json` + `~/.codexctl/active`), so two concurrent herdr HQs race: HQ-A selects
account A, HQ-B runs `use B` before A's lane spawns, and A's lane launches on B. Fix: a launcher
that injects credentials per child process and never touches global state. `use` stays intact.

## 1. Chosen CLI shape

```text
codexctl exec --account <alias> [--] <command> [args...]
```

Runs `<command>` with `CODEX_HOME` pointed at a per-alias exec home seeded with the pinned
profile's credentials. Propagates the child's exit code. Examples:

```bash
codexctl exec --account amir+2@sawmills.ai -- codex -m gpt-5 "prompt"
codexctl exec --account amir+2@sawmills.ai -- codexctl codex -- "prompt"   # pinned + recovery
```

The second form works today with zero wrapper changes: `codexctl codex` already resolves the
child auth file and sessions dir from `CODEX_HOME` when set (`src/commands/codex.rs:976-984`,
`888-893`), and its recovery switch writes only that auth file — `profile::switch_to_auth_json_from`
skips the active marker for non-global paths (`src/profile.rs:226-230`). So recovery inside a
pinned lane rotates accounts privately without ever mutating `~/.codex` or the marker.

Alternatives considered:

- `codexctl codex --account <alias>`: sugar over the same provisioning; conflates pinning with
  recovery policy in one flag surface. Deferred (open question 1); `exec` composition covers it.
- `codexctl use --print-env <alias>` (eval-style exporter): no lifecycle — nothing captures
  refreshed tokens back, and a stale exported snapshot recreates the race. Rejected.
- Positional alias (`codexctl exec <alias> -- cmd`): ambiguous with the command word; `--account`
  is explicit and matches the ticket language. Rejected.

## 2. Credential injection mechanism

What the Codex CLI actually supports:

- **`CODEX_HOME` env (chosen)**: relocates the whole home — `auth.json`, `config.toml`,
  `sessions/`, logs. This repo already relies on it for isolated logins
  (`src/commands/login.rs:25`) and the wrapper already honors it (above). Supported and proven.
- **Env var for credentials**: no Codex env var points at an alternate `auth.json`.
  `OPENAI_API_KEY` switches to API-key auth — the wrong auth mode for ChatGPT-plan seats. Rejected.
- **Config override (`codex -c key=value`)**: the config schema has no auth-file-path key; the
  auth path is always derived from `CODEX_HOME`. Also only reaches a direct `codex` child, not a
  `codexctl codex`-wrapped one. Rejected.

Exec home layout — persistent per alias at `~/.codexctl/exec-homes/<alias>/` (sibling of
`login-homes`, same `store::checked_child` validation):

- `auth.json`: real file, seeded at every launch via `profile::switch_to_auth_json_from(paths,
alias, exec_auth)` — which first folds any leftover exec-home token back into its owning
  profile (subject-matched capture), then installs the profile copy, and skips the marker.
- Every other top-level entry of `~/.codex` (`config.toml`, `AGENTS.md`, `sessions/`,
  `history.jsonl`, ...): symlinked into the exec home at seed time when absent. Config and global
  instructions stay shared; session rollouts land in the real `~/.codex/sessions` through the
  symlink, so `codex resume`, wrapper session discovery, and herdr breadcrumbs keep working.
- Seeding never overwrites an existing non-`auth.json` entry (a symlink codex replaced with a
  real file stays put). Reset path: delete `~/.codexctl/exec-homes/<alias>`.

Persistent-per-alias over ephemeral-per-run: same-alias concurrent runs share one home (same
credentials — benign), a crash cannot lose a refreshed token to cleanup, and it mirrors the
login-homes precedent. Different aliases get disjoint homes — that is the isolation that matters.

## 3. Interaction with use / status / login / token refresh

- `use`, `switch`: untouched. `exec` never writes `~/.codex/auth.json` or `~/.codexctl/active`.
- `status`, `whoami`: the `*` active marker stays truthful. Token refreshes from pinned runs fold
  back into the profile store (below), so the `Token` column stays accurate.
- `login`, `save`: untouched; login-homes and exec-homes are separate trees.
- Token refresh mid-run: codex rewrites `$CODEX_HOME/auth.json` in the exec home. On child exit,
  `exec` captures it back into the owning profile (matched by JWT subject via
  `profile::alias_for_auth_json_from`, copied with `store::atomic_copy` under `store::lock`),
  guarded so it never replaces a profile token with a _strictly earlier-expiring_ same-subject
  token (`api::token_expiry` compare). A crash before capture is healed by the capture built into
  the next seed. Mid-run recovery switches (wrapped form) already capture on every switch.
- All profile-store mutations (seed capture, install, exit capture) run under the existing
  `store.lock`, so concurrent execs serialize their store writes.

## 4. Failure modes

- **Unknown alias**: `profile::get_profile` bails `profile '<alias>' not found` before anything
  is provisioned or spawned. Non-zero exit, no exec home created.
- **Expired access token at launch**: `exec` stays offline (same contract as `use`). It warns via
  `api::is_token_expired` and launches anyway — codex refreshes with the stored refresh token. If
  refresh fails, codex prompts for login _inside the exec home_; a foreign login there is folded
  back only if its subject matches a saved profile, so the store cannot be corrupted.
- **Refresh mid-run writing back**: covered above; the freshness guard (issued-at, then expiry)
  prevents an old exec-home copy from clobbering a newer profile token (e.g. after a re-login
  during the run).
- **Two runs, same alias**: shared exec home, same credentials; seeds serialize on the store
  lock; last refresh wins — same account, no cross-account risk.
- **Codex replaces a symlink** (atomic rename of `config.toml`): file materializes in the exec
  home and drifts from the shared one. Documented; reset by deleting the exec home.
- **`~/.codex` absent**: exec home gets only `auth.json`; sessions then stay per-home. Documented.
- **Child killed by signal**: exit code maps to `128+signal` on unix, else 1.

## 5. File-level implementation plan

1. `src/config.rs`: add `Paths::exec_homes_dir()` → `.codexctl/exec-homes`.
2. `src/store.rs`: add `pub fn exec_home(paths, alias)` mirroring `login_home` (checked child).
3. `src/profile.rs`: add `pub fn capture_exec_auth_from(paths, auth_path)` — subject-matched,
   exp-guarded fold into the owning profile under the store lock (reuses
   `alias_for_auth_json_from`; leaves the existing switch-path capture semantics unchanged).
4. New `src/commands/exec.rs`:
   - `pub fn run(account: &str, args: &[String]) -> Result<i32>`.
   - `provision_exec_home(paths, alias) -> Result<PathBuf>`: validate alias + profile exists,
     seed auth via `switch_to_auth_json_from`, symlink missing top-level `~/.codex` entries.
   - Spawn `std::process::Command` with `.env("CODEX_HOME", home)`, inherited stdio and cwd
     (preserves the AGENTS.md cwd rule); wait; `capture_exec_auth_from`; return status code.
   - Unit tests with `Paths::from_home(tempdir)` and `/bin/sh -c` children.
5. `src/main.rs`: add `Exec { account: String, #[arg(trailing_var_arg, allow_hyphen_values,
num_args(1..))] args: Vec<String> }`; route through `codex_command_outcome` so the child exit
   code becomes the process exit code.
6. `src/commands/mod.rs`: register `pub mod exec;`.
7. `src/commands/completions.rs`: alias completion for `exec --account` (zsh/bash/fish blocks).
8. Docs: README "Pinned launches" section (shape, composition with `codexctl codex`, exec-home
   lifecycle); AGENTS.md map/rules line for `exec` (non-mutating invariant).
9. Gates: `cargo fmt --all -- --check`, `cargo clippy --all-targets`, `cargo test --all-targets`.

## 6. Acceptance tests

Integration (`tests/exec_test.rs`, `assert_cmd` + temp `HOME`, unsigned JWTs with distinct `sub`
claims as tokens):

1. `exec --help` shows `--account` and requires a trailing command.
2. Unknown alias: non-zero exit, stderr names the alias, no `exec-homes/<alias>` created.
3. Seeding: exec-home `auth.json` equals the profile's; pre-existing `~/.codex` entries are
   symlinked (except `auth.json`); `~/.codex/auth.json` and `~/.codexctl/active` byte-identical
   to before.
4. Child env: `exec --account a -- /bin/sh -c 'printf %s "$CODEX_HOME" > out'` lands in
   `exec-homes/a`.
5. Exit-code propagation: child `exit 7` → codexctl exits 7.
6. Capture: child rewrites `$CODEX_HOME/auth.json` with a later-exp same-subject token → profile
   `auth.json` holds it after exit; a foreign-subject token is not folded into any profile; an
   earlier-exp same-subject token does not regress the profile.
7. **Concurrency (the SAW-9631 regression)**: profiles `a` and `b` with distinct tokens; global
   auth holds a third token. Spawn simultaneously
   `exec --account a -- /bin/sh -c 'sleep 1; cat "$CODEX_HOME/auth.json" > out_a'` and the same
   for `b`. Assert `out_a` has exactly token-a, `out_b` exactly token-b, and global auth + active
   marker are unchanged — two simultaneous launches never cross credentials.
8. Session sharing: with a real `~/.codex/sessions` present, a child writing
   `$CODEX_HOME/sessions/x` lands in `~/.codex/sessions/x` (symlink proof).

Manual smoke (needs a live codex): `codexctl exec --account <a> -- codexctl codex -- "hi"`; then
`codexctl whoami` still reports the pre-exec active account.

## 7. Open questions

1. Add `codexctl codex --account <alias>` sugar (same provisioning, one flag) now or as follow-up?
2. Closed in this branch: `captured_auth_supersedes_profile` guards the switch capture too, `iat` then `exp`.
3. Unattended lanes: add `--require-fresh` to fail fast instead of risking an interactive login
   prompt inside the exec home when the token is expired and refresh fails?
4. Symlink policy: is sharing `history.jsonl` and `log/` across accounts desired, or should those
   stay per-alias (allowlist instead of link-everything)?
5. Herdr side (outside this repo): which lane launcher switches to `codexctl exec`, and does HQ
   pass the alias it already selected?
