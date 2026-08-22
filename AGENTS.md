# AGENTS.md

## Project

- `codexctl` is a Rust 2024 CLI for managing multiple OpenAI Codex CLI accounts.
- It saves profiles, switches accounts, reports rate limits, manages banked resets, launches Codex with account recovery, and runs account-pinned commands.
- The package version is `0.1.21`.
- The license is Apache-2.0.

## Map

- `src/main.rs` is the binary entry point.
- `src/lib.rs` is the library entry point.
- `src/commands/` contains CLI command implementations.
- `src/api.rs` contains API code.
- `src/config.rs` and `src/profile.rs` contain configuration and profile code.
- `tests/api_test.rs`, `tests/cli_test.rs`, `tests/config_test.rs`, `tests/exec_test.rs`, and `tests/profile_test.rs` contain tests.
- `docs/superpowers/specs/` contains design specifications.
- `docs/superpowers/plans/` contains implementation runbooks. Use the existing matching plan instead of adding task workflow here.

## Commands

- Check formatting: `cargo fmt --all -- --check`
- Run Clippy: `cargo clippy --all-targets`
- Run tests: `cargo test --all-targets`
- Build a release binary: `cargo build --release`
- Build a locked target release: `cargo build --release --locked --target <target-triple>`
- Install from GitHub: `cargo install --git https://github.com/Sawmills/codexctl`

## Rules

- Preserve isolated login homes at `~/.codexctl/login-homes/<alias>`.
- Preserve pinned exec homes at `~/.codexctl/exec-homes/<alias>`.
- Run Codex login with `CODEX_HOME=~/.codexctl/login-homes/<alias>`.
- Keep `codexctl use` as a local auth-file swap that does not contact OpenAI.
- Keep `codexctl exec` non-mutating. It must never write `~/.codex/auth.json` or the active marker.
- Refuse `codexctl exec` when `CODEX_HOME` is already set. A pinned launch must not replace an inherited Codex home without saying so.
- Capture a token back to the alias the caller named. Fall back to token inspection only when the file does not belong to that alias.
- Pass the pinned alias to children in `CODEXCTL_PINNED_ALIAS`. Recovery must exclude the account a pinned launch already selected.
- Order captured tokens by issued-at and fall back to expiry. Never overwrite a saved token with an older copy of the same seat.
- Pin a launch with `CODEX_HOME=~/.codexctl/exec-homes/<alias>`. Seed `auth.json` as a real file and share every other `~/.codex` entry by symbolic link. Never replace an entry that already exists, so an entry Codex turned into a real file stays as it left it until the home is deleted.
- Never auto-select usage-based accounts during recovery.
- Require confirmation before a switch can bill credits. Refuse that switch on a non-interactive terminal unless `--allow-billing` is set.
- Keep reset approval separate from billing approval. `--allow-billing` must not imply `--allow-resets`.
- Never redeem a banked reset before its account has an exhausted window.
- Spend the qualifying banked reset closest to expiry.
- Keep explicit alias selection from redeeming a reset.
- Preserve the current working directory when `codexctl codex` or `codexctl exec` launches a child.
