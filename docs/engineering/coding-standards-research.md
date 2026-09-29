# Coding standards research: codexctl

Date: 2026-09-29. Source baseline: `02fcd1805e20790c0048ca2c0c707c624b3c3b1f`.

## Repository observations and decisions

The repository is a Rust 2024 account-management CLI. Its risks center on account
identity, credential persistence, consent, child processes, and ambiguous remote
outcomes. `store.rs` uses locks and atomic file replacement. `exec.rs` provisions
a per-alias home and captures refreshed tokens, while preserving live auth and
the active marker. README's blanket claim that pinned launches change no shared
state was broader than this implementation. The correction names the protected
state and the allowed profile update.

Decision: deepen the identity and store operations rather than add a generic
credential repository. Compare a validated account-replacement operation with
separate public compare/copy/mark calls. The first can own ordering and refusal;
the second makes callers recreate the protocol. Keep the concrete implementation
until a real second adapter or test boundary justifies a trait.

`Cargo.toml` declares version 0.1.25 while AGENTS listed 0.1.24. Pointing the guide
to Cargo.toml removes that stale duplicate. This does not change package metadata.
The standards describe repository behavior, not a supported public contract for
OpenAI's private usage or reset endpoints.

## Design sources and adopted interpretation

Matt Pocock's [codebase design vocabulary](https://github.com/mattpocock/skills/blob/c55ee46073ed923f86ce59a5eb3b6d895095d1b7/skills/engineering/codebase-design/SKILL.md),
[deepening guidance](https://github.com/mattpocock/skills/blob/c55ee46073ed923f86ce59a5eb3b6d895095d1b7/skills/engineering/codebase-design/DEEPENING.md),
and [design-it-twice method](https://github.com/mattpocock/skills/blob/c55ee46073ed923f86ce59a5eb3b6d895095d1b7/skills/engineering/codebase-design/DESIGN-IT-TWICE.md)
support small interfaces that hide meaningful complexity, public-behavior tests,
and comparison of alternative designs. The repository decisions above apply
those ideas to existing code. They do not require a new directory layout,
dependency, universal interface, or speculative abstraction.

A review-only AGENTS pointer makes the standards available during review without
turning a long standards file into unconditional task setup. The standards are
review requirements, not evidence that every existing path already satisfies them.

## Stack sources

[Rust Command](https://doc.rust-lang.org/std/process/struct.Command.html) provides
structured arguments and child-specific environment control. [Rust rename](https://doc.rust-lang.org/std/fs/fn.rename.html)
documents platform-dependent replacement behavior. The repository adds locking,
private permissions, and synchronization around replacement. Those extra
properties come from source and tests, not from rename alone.

## Pinned repository evidence

- [Cargo.toml](https://github.com/Sawmills/codexctl/blob/02fcd1805e20790c0048ca2c0c707c624b3c3b1f/Cargo.toml)
- [src/store.rs](https://github.com/Sawmills/codexctl/blob/02fcd1805e20790c0048ca2c0c707c624b3c3b1f/src/store.rs)
- [src/profile.rs](https://github.com/Sawmills/codexctl/blob/02fcd1805e20790c0048ca2c0c707c624b3c3b1f/src/profile.rs)
- [src/commands/exec.rs](https://github.com/Sawmills/codexctl/blob/02fcd1805e20790c0048ca2c0c707c624b3c3b1f/src/commands/exec.rs)
- [src/commands/use_profile.rs](https://github.com/Sawmills/codexctl/blob/02fcd1805e20790c0048ca2c0c707c624b3c3b1f/src/commands/use_profile.rs)
- [.github/workflows/ci.yml](https://github.com/Sawmills/codexctl/blob/02fcd1805e20790c0048ca2c0c707c624b3c3b1f/.github/workflows/ci.yml)

## Evidence limits

This research reads code and primary documentation. It does not establish live
service, production, or external-provider acceptance. Future affected changes
need the real dependency evidence specified by the standards. Documentation
changes do not require unrelated runtime refactoring or production probes.
