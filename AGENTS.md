# AGENTS.md

## Project

- `codexctl` is a Rust 2024 CLI for managing multiple OpenAI Codex CLI accounts.
- It saves profiles, switches accounts, reports rate limits, manages banked resets, and launches Codex with account recovery.
- The package version is `0.1.16`.
- The license is Apache-2.0.

## Map

- `src/main.rs` is the binary entry point.
- `src/lib.rs` is the library entry point.
- `src/commands/` contains CLI command implementations.
- `src/api.rs` contains API code.
- `src/config.rs` and `src/profile.rs` contain configuration and profile code.
- `tests/api_test.rs`, `tests/cli_test.rs`, `tests/config_test.rs`, and `tests/profile_test.rs` contain tests.
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
- Run Codex login with `CODEX_HOME=~/.codexctl/login-homes/<alias>`.
- Keep `codexctl use` as a local auth-file swap that does not contact OpenAI.
- Never auto-select usage-based accounts during recovery.
- Require confirmation before a switch can bill credits. Refuse that switch on a non-interactive terminal unless `--allow-billing` is set.
- Keep reset approval separate from billing approval. `--allow-billing` must not imply `--allow-resets`.
- Never redeem a banked reset before its account has an exhausted window.
- Spend the qualifying banked reset closest to expiry.
- Keep explicit alias selection from redeeming a reset.
- Preserve the current working directory when `codexctl codex` launches Codex.
