# Linux ARM64 Release Builds

Date: 2026-10-02. Audience: codexctl release maintainers.

Use `ubuntu-22.04-arm` to build `aarch64-unknown-linux-gnu` natively.
This keeps the new ARM64 release build on the Ubuntu 22.04 baseline.

GitHub lists `ubuntu-22.04-arm` as a standard ARM64 runner for public
repositories. A live `gh repo view Sawmills/codexctl --json visibility`
returned `PUBLIC` during this check.
[GitHub runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)

Ubuntu 22.04's updated `libc6` package uses glibc 2.35. The ARM runner image
includes Cargo, Rust, GCC, and Clang. Rust supports
`aarch64-unknown-linux-gnu` as a Tier 1 target with native host tools.
These sources support reusing the current Cargo build without a cross compiler.
[Ubuntu package](https://packages.ubuntu.com/jammy-updates/libc6),
[ARM image inventory](https://github.com/actions/partner-runner-images/blob/main/images/arm-ubuntu-22-image.md),
[Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html)

The repository pins actionlint 1.7.7 in `.trunk/trunk.yaml`. That version
recognizes `ubuntu-22.04-arm`, so this runner needs no lint suppression.
[Pinned actionlint source](https://github.com/rhysd/actionlint/blob/v1.7.7/rule_runner_label.go#L38-L41)

## Proposed Compatibility Check

After building, check that `getconf GNU_LIBC_VERSION` reports `glibc 2.35`
on the ARM64 runner. Run `codexctl --version` and `codexctl-central --help`
before packaging. This checks that the actual binaries start on the selected baseline.
Keep the existing locked build command:

```sh
cargo build --release --locked --target aarch64-unknown-linux-gnu
```

This research establishes runner and toolchain suitability. The release
workflow must still demonstrate a successful native build and startup.
Startup checks cover loading and argument parsing; they do not establish
compatibility across every Linux distribution or exercise account operations.
